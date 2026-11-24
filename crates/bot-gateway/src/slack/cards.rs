//! 소유자 DM 승인 카드 전달(⑧)의 게이트웨이 몫: 전달 루프와 클릭 대조. 포트·타입은 `bot_core::cards`다. 계약은 docs/reliability.md «Slack 소유자에게 승인 카드를 전달할 때».
//!
//! - `CardDeliverer`가 저장소의 전달 작업을 점유(`claim`)하고, 현재 활성 판본으로 원래 바인딩이 아직 이 설치의 Slack 채널 바인딩인지·소유자가 아직 멤버인지·카드가 그 승인의 것인지
//!   확인한 뒤 `begin`이 Ok일 때만(**최초 전송자만**) 소유자 DM에 카드를 **한 번** 올리고(`SlackSender::post_owner_card`) 결과를 `settle`한다.
//!   올라갔는지 불명이면 `Held`로 정산하고 자동으로 다시 올리지 않는다. 열 수 없거나 올릴 수 없으면 저장소가 `approval_dm_unavailable`로 닫는다.
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
    id::{ApprovalId, BindingId},
    port::Error,
    profile::{Adapter, Snapshot},
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

/// 올렸거나 닫았는데 저장소가 정산을 받지 못한 결과: 저장소가 답할 때까지 같은 결과로 다시 기록한다(같은 정산의 재전달은 같은 결과다). 카드를 다시 올리지 않는다.
struct Unsettled {
    nonce: ApprovalId,
    outcome: CardOutcome,
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

    async fn deliver_one(&self, job: CardJob) {
        let nonce = job.nonce.clone();
        // 현재 활성 프로필·원래 바인딩·소유자: 활성 프로필이 아니면 건드리지 않는다(리스가 끝나면 저장소가 다시 잡게 둔다)
        let checked = {
            let Ok(routes) = self.ingress.routes() else {
                return;
            };
            let Some(snapshot) = routes.snapshot(&job.profile) else {
                return;
            };
            // 같은 설치의 Slack 채널 바인딩이고 소유자가 지금도 멤버여야 한다(아니면 올리지 않고 닫는다)
            channel_workspace(snapshot, &job.binding)
                .filter(|_| snapshot.members.contains_key(&job.owner))
                .map(|workspace| (workspace.to_owned(), snapshot.revision.clone()))
        };
        let card = serde_json::from_str(&job.card.0)
            .ok()
            .and_then(|value| parse_card(&value).ok())
            .filter(|card| card.nonce == job.nonce.0);
        let (Some((workspace, revision)), Some(card)) = (checked, card) else {
            self.settle(nonce, CardOutcome::Unavailable).await;
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
        self.settle(nonce, outcome_of(posted)).await;
    }

    /// 결과를 기록한다. 저장소가 일시적으로 받지 못하면(`Unavailable`) 같은 결과를 나중에 다시 기록한다 — 이미 올린 카드를 다시 올리지 않는다.
    async fn settle(&self, nonce: ApprovalId, outcome: CardOutcome) {
        self.unsettled
            .lock()
            .unwrap()
            .push(Unsettled { nonce, outcome });
        self.resettle().await;
    }

    /// 아직 기록하지 못한 결과를 모두 기록해 본다. 저장소가 받지 못한 것(`Unavailable`)만 남긴다.
    async fn resettle(&self) {
        let waiting = std::mem::take(&mut *self.unsettled.lock().unwrap());
        for item in waiting {
            match self
                .jobs
                .settle(item.nonce.clone(), item.outcome.clone())
                .await
            {
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
