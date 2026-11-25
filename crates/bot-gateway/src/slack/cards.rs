//! 소유자 DM 승인 카드 전달(⑧)의 게이트웨이 몫: 전달 루프와 클릭 대조. 포트·타입은 `bot_core::cards`다. 계약은 docs/reliability.md «Slack 소유자에게 승인 카드를 전달할 때».
//!
//! - `CardDeliverer`가 저장소의 전달 작업을 점유(`claim`)하고, 현재 활성 판본으로 원래 바인딩이 아직 이 설치의 Slack 채널 바인딩인지·소유자가 아직 멤버인지·카드가 그 승인의 것인지
//!   확인한 뒤 `begin`이 Ok일 때만(**최초 전송자만**) 소유자 DM에 카드를 **한 번** 올리고(`SlackSender::post_owner_card`) 결과를 `settle`한다.
//!   올라갔는지 불명이면 `Held`로 정산하고 자동으로 다시 올리지 않는다. 열 수 없거나 올릴 수 없으면 저장소가 `approval_dm_unavailable`로 닫는다.
//! - **HTTP(OpenAI 호환) 출처 카드**(`CardOutbox::origin`이 `Some`)는 채널에서 시작한 카드가 아니다: `CardJob::binding`은 전달 바인딩(같은 설치의 Slack 채널 바인딩)이고 `owner`는 지정된 승인자다.
//!   올리기 전에 접수 때 고정한 것(원래 OpenAI 바인딩·요청자·판본·전달 설치 `team_id`)과 지금을 모두 대조한다 — 고정 판본의 승인 경로, 지금 판본의 승인 경로, 카드의 전달 바인딩·승인자,
//!   호스트가 그 워크스페이스에 연결한 팀이 같아야 한다. 어긋나면 올리지 않고 `block`으로 저장된 판정(`approver_revoked`·`route_changed`)을 남긴다. 증거를 읽지 못하면 올리지 않고 건드리지 않는다
//!   (리스가 끝나면 다시 잡힌다). 증거가 있는 카드는 어떤 경우에도 채널 출처 경로로 되돌아가지 않는다.
//! - 클릭은 승인에 붙은 영수증(`StoredApproval::delivery`, 게시를 확인한 뒤에만 있다)과 대조한다(`is_the_card`): 설치의 팀·DM 채널·메시지 `ts`·소유자·프로필·원래 바인딩이 모두 같아야 하고,
//!   그때는 DM 입력 바인딩이 없어도 이 카드의 버튼만 처리한다(일반 DM 입구를 열지 않는다).
use super::{
    frame::Button,
    send::{CardPost, SlackSender},
    socket::Installation,
};
use crate::{identity, ingress::Ingress, reply::parse_card};
pub use bot_core::cards::{CardJob, CardOutbox, CardOutcome, CardReceipt};
use bot_core::{
    api::ApprovalBlock,
    cards::CardOrigin,
    id::{AgentId, ApprovalId, BindingId, PrincipalId, Revision},
    port::Error,
    profile::{Adapter, ApprovalRoute, Snapshot},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const BATCH: u32 = 8;
const IDLE: Duration = Duration::from_millis(500);
const BACKOFF: Duration = Duration::from_secs(2);
/// 한 건이 이보다 오래 걸리면 올라갔는지 알 수 없는 것으로 정산한다.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// 이 바인딩이 지금도 Slack 채널 바인딩이면 그 워크스페이스 별칭(카드를 올릴 설치)
fn channel_workspace<'a>(snapshot: &'a Snapshot, binding: &BindingId) -> Option<&'a str> {
    match snapshot.bindings.get(binding).map(|b| &b.source) {
        Some(Adapter::SlackChannel { workspace, .. }) => Some(workspace),
        _ => None,
    }
}

/// 이 클릭이 저장된 영수증의 **그 카드**의 버튼인가: 설치의 프로필·팀, 클릭한 DM 채널·메시지 `ts`, 클릭한 사람이 영수증의 소유자이고,
/// 영수증의 원래 바인딩이 지금도 이 설치(워크스페이스)의 Slack 채널 바인딩이어야 한다.
/// 메시지를 복사한 곳·다른 DM·다른 사용자·다른 팀·다른 프로필·다른 설치의 클릭은 아니다. `snapshot`은 설치 프로필의 현재 활성 판본이다.
pub fn is_the_card(
    receipt: &CardReceipt,
    install: &Installation,
    snapshot: &Snapshot,
    button: &Button,
) -> bool {
    !receipt.ts.is_empty()
        && receipt.profile == install.profile
        && receipt.team == install.team
        && button.team == install.team
        && button.channel == receipt.dm
        && button.message_ts == receipt.ts
        && identity::slack(&install.alias, &button.user).as_ref() == Some(&receipt.owner)
        && channel_workspace(snapshot, &receipt.binding) == Some(install.alias.as_str())
}

/// 저장소에 기록할 결과: 전달 결과 또는 승인 경로가 어긋나 닫는다는 판정
#[derive(Clone)]
enum Settlement {
    Outcome(CardOutcome),
    Block(ApprovalBlock),
}

/// 올렸거나 닫았는데 저장소가 정산을 받지 못한 결과: 저장소가 답할 때까지 같은 결과로 다시 기록한다(같은 정산의 재전달은 같은 결과다). 카드를 다시 올리지 않는다.
struct Unsettled {
    nonce: ApprovalId,
    settlement: Settlement,
}

/// 점유한 작업을 어떻게 할 것인가
enum Verdict {
    /// 올린다: 카드를 올릴 설치(워크스페이스 별칭)와 확인한 현재 판본
    Post(String, Revision),
    /// 올리지 않고 `approval_dm_unavailable`로 닫는다
    Close,
    /// 승인 경로가 어긋나 올리지 않고 닫는다(저장된 판정을 남긴다)
    Block(ApprovalBlock),
    /// 지금은 건드리지 않는다(활성 프로필이 아니거나 증거를 읽지 못했다 — 리스가 끝나면 다시 잡힌다)
    Leave,
}

/// 한 판본이 원래 요청(OpenAI 바인딩 + 인증된 요청자)에 대해 가리키는 사실: 입구·공개 모델·에이전트와 승인 경로.
struct Origin {
    endpoint: String,
    model: String,
    agent: AgentId,
    route: ApprovalRoute,
}

/// 이 판본에서 `binding`을 통해 `requester`가 가진 사실. 바인딩이 OpenAI 모델이 아니거나 승인 경로가 어긋나면 None.
fn origin_in(snapshot: &Snapshot, binding: &BindingId, requester: &PrincipalId) -> Option<Origin> {
    let route = snapshot.approval_route(binding, requester)?;
    let found = snapshot.bindings.get(binding)?;
    let Adapter::OpenAi {
        endpoint, model, ..
    } = &found.source
    else {
        return None;
    };
    Some(Origin {
        endpoint: endpoint.clone(),
        model: model.clone(),
        agent: found.agent.clone(),
        route,
    })
}

/// HTTP 출처 카드가 접수 때 고정한 것과 지금 모두 같은 일을 가리키는가: 카드가 가리키는 사람·전달 바인딩이 승인 경로이고, 원래 요청의 입구·모델·에이전트가 그대로여야 한다
/// (승인자·전달 바인딩·팀이 같아도 모델이나 에이전트가 재지정됐으면 카드에 담긴 옛 도구 인자는 새 대상의 것이 아니다). 어긋남의 이유는 닫힌 집합이다:
/// 승인 지정(매핑)이나 사람 멤버가 철회됐으면 `approver_revoked`, 그 밖에 대상·전달 경로·바인딩·설치가 달라졌거나 확인할 수 없으면 `route_changed`.
fn route_problem(
    pinned: Option<&Snapshot>,
    current: &Snapshot,
    origin: &CardOrigin,
    job: &CardJob,
) -> Option<ApprovalBlock> {
    let facts = |snapshot: &Snapshot| origin_in(snapshot, &origin.binding, &origin.requester);
    let card_route = |facts: &Origin| matches!(&facts.route, ApprovalRoute::Slack { person, binding } if *person == job.owner && *binding == job.binding);
    // 접수 때 고정한 판본에서 카드가 가리키는 바로 그 경로였는가(고정 판본을 복원하지 못하면 확인할 수 없다)
    let Some(then) = pinned.and_then(facts).filter(card_route) else {
        return Some(ApprovalBlock::RouteChanged);
    };
    // 지금도 같은 경로이고 원래 요청이 가리키던 것이 그대로인가
    if let Some(now) = facts(current).filter(card_route) {
        let same =
            (&now.endpoint, &now.model, &now.agent) == (&then.endpoint, &then.model, &then.agent);
        return (!same).then_some(ApprovalBlock::RouteChanged);
    }
    let mapped = match current.bindings.get(&origin.binding).map(|b| &b.source) {
        Some(Adapter::OpenAi { approvers, .. })
            if current.members.contains_key(&origin.requester) =>
        {
            approvers.get(&origin.requester)
        }
        _ => return Some(ApprovalBlock::RouteChanged),
    };
    match mapped {
        Some(route) if current.members.contains_key(route.person()) => {
            Some(ApprovalBlock::RouteChanged)
        }
        _ => Some(ApprovalBlock::ApproverRevoked),
    }
}

pub struct CardDeliverer {
    jobs: Arc<dyn CardOutbox>,
    ingress: Arc<Ingress>,
    sender: SlackSender,
    unsettled: Mutex<Vec<Unsettled>>,
}

impl CardDeliverer {
    pub fn new(jobs: Arc<dyn CardOutbox>, ingress: Arc<Ingress>, sender: SlackSender) -> Self {
        Self {
            jobs,
            ingress,
            sender,
            unsettled: Mutex::new(Vec::new()),
        }
    }

    /// 한 번 훑는다(먼저 정산하지 못한 결과를 다시 기록한다). 처리한 작업 수를 돌려준다.
    pub async fn tick(&self) -> Result<usize, Error> {
        self.resettle().await;
        let jobs = self.jobs.claim(BATCH).await?;
        let count = jobs.len();
        for job in jobs {
            self.deliver_one(job).await;
        }
        Ok(count)
    }

    /// stop까지 돈다.
    pub async fn run(&self, stop: CancellationToken) {
        loop {
            let pause = match self.tick().await {
                Ok(n) if n >= BATCH as usize => Duration::ZERO,
                Ok(_) => IDLE,
                Err(error) => {
                    tracing::warn!(?error, "승인 카드 전달 작업을 잡지 못했어요");
                    BACKOFF
                }
            };
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(pause) => {}
            }
        }
    }

    /// 점유한 작업을 올릴지·닫을지·건드리지 않을지 정한다. 현재 활성 프로필·바인딩·소유자 기준이다.
    async fn judge(&self, job: &CardJob) -> Verdict {
        // HTTP 출처의 고정 증거. 읽지 못하면 채널 출처인지 알 수 없으므로 올리지 않는다
        let Ok(origin) = self.jobs.origin(job.nonce.clone()).await else {
            return Verdict::Leave;
        };
        let Ok(routes) = self.ingress.routes() else {
            return Verdict::Leave;
        };
        let Some(snapshot) = routes.snapshot(&job.profile) else {
            return Verdict::Leave;
        };
        let workspace = channel_workspace(snapshot, &job.binding);
        let Some(origin) = origin else {
            // 채널 출처: 같은 설치의 Slack 채널 바인딩이고 소유자가 지금도 멤버여야 한다(아니면 올리지 않고 닫는다)
            return match workspace.filter(|_| snapshot.members.contains_key(&job.owner)) {
                Some(workspace) => Verdict::Post(workspace.to_owned(), snapshot.revision.clone()),
                None => Verdict::Close,
            };
        };
        let pinned = self.ingress.pinned(&job.profile, &origin.revision);
        if let Some(why) = route_problem(pinned.as_deref(), snapshot, &origin, job) {
            return Verdict::Block(why);
        }
        // 접수 때 호스트가 확인한 설치(팀)와 지금 호스트가 그 워크스페이스에 연결한 팀이 같아야 한다(없으면 일반 채널 경로로 되돌아가지 않는다)
        let team_now = workspace.and_then(|alias| self.ingress.host().slack.workspaces.get(alias));
        match (workspace, team_now, origin.team.as_deref()) {
            (Some(workspace), Some(now), Some(then)) if now.team_id == then => {
                Verdict::Post(workspace.to_owned(), snapshot.revision.clone())
            }
            _ => Verdict::Block(ApprovalBlock::RouteChanged),
        }
    }

    async fn deliver_one(&self, job: CardJob) {
        let nonce = job.nonce.clone();
        let (workspace, revision) = match self.judge(&job).await {
            Verdict::Leave => return,
            Verdict::Close => {
                self.settle(nonce, Settlement::Outcome(CardOutcome::Unavailable))
                    .await;
                return;
            }
            Verdict::Block(why) => {
                self.settle(nonce, Settlement::Block(why)).await;
                return;
            }
            Verdict::Post(workspace, revision) => (workspace, revision),
        };
        let card = serde_json::from_str(&job.card.0)
            .ok()
            .and_then(|value| parse_card(&value).ok())
            .filter(|card| card.nonce == job.nonce.0);
        let Some(card) = card else {
            self.settle(nonce, Settlement::Outcome(CardOutcome::Unavailable))
                .await;
            return;
        };
        // 최초 전송자만 올린다. 실패하거나 응답을 잃으면 올리지 않는다
        if self.jobs.begin(nonce.clone(), revision).await.is_err() {
            return;
        }
        let posted = tokio::time::timeout(
            SEND_TIMEOUT,
            self.sender
                .post_owner_card(&job.profile, &workspace, &job.owner, &card),
        )
        .await
        .unwrap_or(CardPost::Held);
        self.settle(nonce, Settlement::Outcome(outcome_of(posted)))
            .await;
    }

    /// 결과를 기록한다. 저장소가 일시적으로 받지 못하면(`Unavailable`) 같은 결과를 나중에 다시 기록한다 — 이미 올린 카드를 다시 올리지 않는다.
    async fn settle(&self, nonce: ApprovalId, settlement: Settlement) {
        self.unsettled
            .lock()
            .unwrap()
            .push(Unsettled { nonce, settlement });
        self.resettle().await;
    }

    /// 아직 기록하지 못한 결과를 모두 기록해 본다. 저장소가 받지 못한 것(`Unavailable`)만 남긴다.
    async fn resettle(&self) {
        let waiting = std::mem::take(&mut *self.unsettled.lock().unwrap());
        for item in waiting {
            let recorded = match &item.settlement {
                Settlement::Outcome(outcome) => {
                    self.jobs.settle(item.nonce.clone(), outcome.clone()).await
                }
                Settlement::Block(why) => self.jobs.block(item.nonce.clone(), *why).await,
            };
            match recorded {
                Ok(()) => {}
                Err(Error::Unavailable) => self.unsettled.lock().unwrap().push(item),
                // 다른 결과가 이미 기록됐거나(`Conflict`) 낡았다(`Stale`): 다시 보내도 같다
                Err(error) => {
                    tracing::error!(?error, "승인 카드 전달 정산이 받아들여지지 않았어요");
                }
            }
        }
    }
}

fn outcome_of(posted: CardPost) -> CardOutcome {
    match posted {
        CardPost::Posted { team, dm, ts } => CardOutcome::Posted { team, dm, ts },
        CardPost::NotSent {
            reason,
            retry_after,
        } => CardOutcome::NotSent {
            reason,
            retry_after,
        },
        CardPost::Unavailable => CardOutcome::Unavailable,
        CardPost::Held => CardOutcome::Held,
    }
}
