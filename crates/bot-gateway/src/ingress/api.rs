//! OpenAI 호환 입구의 접수와 재확인. 어댑터가 키로 인증한 HTTP 주체가 공개 모델을 골라 `ApiRuns`로 접수하거나 같은 키의 기존 작업에 붙는다.
//!
//! 순서: 입구 별칭의 프로필(멤버가 아니면 403) → 모델의 바인딩(없거나 이 주체가 쓸 수 없으면 «없음») → 활성 판본 확인 → 입구·주체 한도 → 원자 접수.
//! 모델 가시성은 멤버 역할 ∩ 프로필 정책 ∩ 에이전트 상한이 `run`을 덮는지다(`scope::effective`). 트리거·도구 직접 실행·본문의 에이전트 선택은 없다 — 바인딩 에이전트를 직접 접수한다.
use super::{Admission, Ingress, Rejection, Respond};
use crate::{
    entrance::Real,
    inbound::Event,
    observe::{self, Intake},
    routes::Routes,
};
use bot_core::{
    api::{ApiAccept, ApiAdmission, ApiRun, ApiRuns, ChatBody},
    command::{Accept, Envelope, HttpRoute, Target},
    id::{BindingId, PrincipalId, ProfileId, RequestId},
    port::Error,
    profile::{Adapter, ApprovalRoute, ResponseMode, Snapshot},
    scope::{RUN_SCOPE, effective},
    state::{Conversation, ConversationKind},
    trace::now_nano,
};
use std::time::Duration;

/// 접수 경로(요청 순서와 무관하게 고정). 스팬과 접수 명령의 HTTP 경로로 쓴다.
pub const CHAT_PATH: &str = "/v1/chat/completions";

/// 키로 인증한 주체의 접수 요청. 본문은 입구가 검증한 대화다.
pub struct ApiInbound {
    pub endpoint: String,
    /// 호출자가 본 공개 모델 이름
    pub model: String,
    pub principal: PrincipalId,
    /// 적용한 멱등 키
    pub request: RequestId,
    pub body: ChatBody,
    pub digest: [u8; 32],
    /// POST마다 서버가 정한 새 대화 식별자(재전달에서는 다르다)
    pub conversation: String,
}

#[derive(Debug)]
pub enum ApiVerdict {
    /// 새로 접수했거나 같은 키·같은 지문의 기존 작업에 붙었다.
    Admitted(Box<ApiRun>),
    /// 같은 키인데 다른 지문이다.
    Conflict,
    /// 인증된 키지만 이 프로필의 멤버가 아니다.
    NotMember,
    /// 없는 모델이거나 이 주체가 쓸 수 없는 모델이다(둘은 구별하지 않는다).
    Hidden,
    RateLimited(Duration),
    /// 새 작업인데 입구의 실행 수 한도가 찼다(아무것도 기록하지 않았다).
    Full,
    /// 확실히 커밋 전에 실패했다(구판 프로세스·저장소 장애 포함).
    Unavailable,
    /// 커밋했는지 알 수 없다. 호출자가 같은 키로 조회해 확인한다.
    Unknown,
}

/// 결과·진행 안내를 내보내기 직전의 재확인 결과
#[derive(Debug, Eq, PartialEq)]
pub enum ApiRelease {
    Allowed,
    /// 이제 멤버가 아니다.
    NotMember,
    /// 모델이 없어졌거나, 접수했던 바인딩·에이전트가 아닌 것으로 재지정됐거나, 이 주체가 더는 쓸 수 없다.
    Hidden,
    /// 확인하지 못했다(활성 집합을 읽지 못했거나 구판 프로세스). 결과를 가린다.
    Unconfirmed,
}

/// 스팬에 쓸 비민감 정보
#[derive(Default)]
struct Seen {
    profile: Option<ProfileId>,
    binding: Option<String>,
}

impl Ingress {
    pub async fn admit_api(
        &self,
        api: &dyn ApiRuns,
        routes: &Routes,
        inbound: ApiInbound,
    ) -> ApiVerdict {
        let started = now_nano();
        let mut seen = Seen::default();
        let verdict = self.admit_api_inner(api, routes, &inbound, &mut seen).await;
        if let (Some(profile), Some(binding)) = (&seen.profile, &seen.binding) {
            let event = Event::Http {
                method: "POST".to_owned(),
                path: CHAT_PATH.to_owned(),
            };
            observe::record_intake(
                &*self.tracer,
                &Intake {
                    profile,
                    binding,
                    request: &inbound.request.0,
                    principal: &inbound.principal.0,
                    conversation: "http",
                    trigger: None,
                    private: false,
                    route: Some(("POST", CHAT_PATH)),
                    adapter: "http",
                    event: &event,
                    started,
                },
                &mirror(&verdict),
            );
        }
        verdict
    }

    async fn admit_api_inner(
        &self,
        api: &dyn ApiRuns,
        routes: &Routes,
        inbound: &ApiInbound,
        seen: &mut Seen,
    ) -> ApiVerdict {
        let Some(profile) = routes.openai_profile(&inbound.endpoint) else {
            return ApiVerdict::Hidden;
        };
        let Some(snapshot) = routes.snapshot(profile) else {
            return ApiVerdict::Hidden;
        };
        seen.profile = Some(profile.clone());
        if !snapshot.members.contains_key(&inbound.principal) {
            return ApiVerdict::NotMember;
        }
        let real = Real::HttpModel {
            endpoint: inbound.endpoint.clone(),
            model: inbound.model.clone(),
        };
        let Some(route) = routes.find(&real) else {
            return ApiVerdict::Hidden;
        };
        seen.binding = Some(route.binding_id.0.clone());
        let scopes = match effective(snapshot, &inbound.principal, &route.binding.agent) {
            Some(scopes) if scopes.contains(RUN_SCOPE) => scopes,
            _ => return ApiVerdict::Hidden,
        };
        // 구판 프로세스는 새 접수를 하지 않는다(한도 검사 전에 닫아 SDK가 다시 보내게 한다)
        if self.verify_active().await.is_err() {
            return ApiVerdict::Unavailable;
        }
        let entrance_key = format!("{}/{}", profile.0, route.binding_id.0);
        if let Err(wait) = self.entrance_limiter.check(&entrance_key) {
            return ApiVerdict::RateLimited(wait);
        }
        let principal_key = format!("{}/{}", profile.0, inbound.principal.0);
        if let Err(wait) = self.principal_limiter.check(&principal_key) {
            return ApiVerdict::RateLimited(wait);
        }
        let Some(question) = inbound
            .body
            .current()
            .and_then(|at| inbound.body.turns.get(at))
        else {
            return ApiVerdict::Unavailable;
        };
        let accept = Accept {
            reply_transport: crate::routes::reply_transport(&route.binding.source),
            // 기록용: 승인 경로가 없으면 None이다. 실제 경로는 브로커가 고정 판본·현재 판본에서 다시 풀어 확인한다.
            approver: snapshot
                .approval_route(route.binding_id, &inbound.principal)
                .map(|route| route.person().clone()),
            revision: snapshot.revision.clone(),
            target: Target::Agent {
                agent: route.binding.agent.clone(),
                instruction: question.content.clone(),
            },
            scopes,
            event: Envelope {
                profile: profile.clone(),
                request: inbound.request.clone(),
                binding: route.binding_id.clone(),
                principal: inbound.principal.clone(),
                conversation: Conversation {
                    kind: ConversationKind::Http,
                    id: inbound.conversation.clone(),
                    thread: Some(inbound.conversation.clone()),
                },
                http: Some(HttpRoute {
                    method: "POST".to_owned(),
                    path: CHAT_PATH.to_owned(),
                    response: ResponseMode::Accepted,
                }),
                attachments: Vec::new(),
                body: inbound.body.to_payload(),
            },
        };
        let request = ApiAccept {
            accept,
            endpoint: inbound.endpoint.clone(),
            model: inbound.model.clone(),
            digest: inbound.digest,
            max_active: self.api_max_active(&inbound.endpoint),
            team: self.delivery_team(snapshot, route.binding_id, &inbound.principal),
        };
        match api.accept(request).await {
            Ok(ApiAdmission::Accepted(run) | ApiAdmission::Replayed(run)) => {
                ApiVerdict::Admitted(Box::new(run))
            }
            Ok(ApiAdmission::Conflict) => ApiVerdict::Conflict,
            Ok(ApiAdmission::Capacity) => ApiVerdict::Full,
            Ok(ApiAdmission::Stale) => ApiVerdict::Unavailable,
            Ok(ApiAdmission::Unknown) => ApiVerdict::Unknown,
            Err(Error::Conflict) => ApiVerdict::Conflict,
            Err(Error::Denied) => ApiVerdict::NotMember,
            // 포트가 `Err`로 돌려주는 것은 확실히 커밋 전의 실패뿐이다(커밋을 확정하지 못하면 `Unknown`이다)
            Err(Error::Invalid) => {
                // 게이트웨이가 만든 접수 명령을 저장소가 받아들이지 않았다: 호출자의 잘못이 아니라 조립 오류다(내용은 싣지 않는다)
                tracing::error!(endpoint = %inbound.endpoint, "저장소가 OpenAI 접수 명령을 거절했어요(Invalid)");
                ApiVerdict::Unavailable
            }
            Err(Error::Unavailable | Error::Stale | Error::Missing) => ApiVerdict::Unavailable,
        }
    }

    /// 사람 승인 카드가 Slack DM으로 갈 때 접수 순간 호스트가 확인한 전달 설치(`team_id`). 요청에 묶어 두면 같은 별칭의 실제 팀이 바뀐 재배포 뒤
    /// 옛 요청의 카드가 새 팀에 올라가지 않는다. 승인 경로가 로컬이거나 없으면, 또는 호스트가 그 워크스페이스를 모르면 None이다.
    fn delivery_team(
        &self,
        snapshot: &Snapshot,
        binding: &BindingId,
        principal: &PrincipalId,
    ) -> Option<String> {
        let ApprovalRoute::Slack {
            binding: delivery, ..
        } = snapshot.approval_route(binding, principal)?
        else {
            return None;
        };
        let Adapter::SlackChannel { workspace, .. } = &snapshot.bindings.get(&delivery)?.source
        else {
            return None;
        };
        let workspace = self.host.slack.workspaces.get(workspace)?;
        Some(workspace.team_id.clone())
    }

    /// 입구별 진행 중 작업 수 한도(호스트 설정). 검증을 통과한 설정이라 1..=256이다.
    fn api_max_active(&self, endpoint: &str) -> u32 {
        self.host
            .http
            .openai
            .get(endpoint)
            .map_or(crate::host::DEFAULT_MAX_ACTIVE_RUNS, |e| e.max_active())
    }

    /// 결과·진행 안내를 내보내기 직전의 재확인: 지금의 활성 집합에서도 이 주체가 멤버이고, 같은 모델 이름이 **접수했던 바인딩·에이전트**를 가리키며
    /// 이 주체가 아직 쓸 수 있고, 이 프로세스의 판본이 저장소의 활성 판본인가. 확인하지 못하면 결과를 가린다.
    pub async fn recheck_api(
        &self,
        endpoint: &str,
        principal: &PrincipalId,
        run: &ApiRun,
    ) -> ApiRelease {
        // 기다림이 필요한 확인(활성 집합)을 먼저 하고, 표는 그 **뒤에** 읽어 판정한다: 판정에는 기다림이 없어 읽은 표가 낡을 틈이 없다
        if self.verify_active().await.is_err() {
            return ApiRelease::Unconfirmed;
        }
        let Ok(routes) = self.routes() else {
            return ApiRelease::Unconfirmed;
        };
        let Some(profile) = routes.openai_profile(endpoint) else {
            return ApiRelease::Hidden;
        };
        let Some(snapshot) = routes.snapshot(profile) else {
            return ApiRelease::Hidden;
        };
        if !snapshot.members.contains_key(principal) {
            return ApiRelease::NotMember;
        }
        let real = Real::HttpModel {
            endpoint: endpoint.to_owned(),
            model: run.model.clone(),
        };
        let Some(route) = routes.find(&real) else {
            return ApiRelease::Hidden;
        };
        if *route.binding_id != run.binding || route.binding.agent != run.agent {
            return ApiRelease::Hidden;
        }
        let runnable = effective(snapshot, principal, &route.binding.agent)
            .is_some_and(|scopes| scopes.contains(RUN_SCOPE));
        if runnable {
            ApiRelease::Allowed
        } else {
            ApiRelease::Hidden
        }
    }
}

/// 스팬은 기존 접수와 같은 모양의 결과로 기록한다.
fn mirror(verdict: &ApiVerdict) -> Admission {
    match verdict {
        ApiVerdict::Admitted(run) => Admission::Accepted {
            run: run.run.clone(),
            respond: Respond::Accepted,
        },
        ApiVerdict::Conflict => Admission::Rejected(Rejection::Conflict),
        ApiVerdict::NotMember => Admission::Rejected(Rejection::Unregistered),
        ApiVerdict::Hidden => Admission::Rejected(Rejection::NoRoute),
        ApiVerdict::RateLimited(wait) => Admission::Rejected(Rejection::RateLimited(*wait)),
        ApiVerdict::Full => Admission::Rejected(Rejection::RateLimited(Duration::from_secs(1))),
        ApiVerdict::Unavailable | ApiVerdict::Unknown => {
            Admission::Rejected(Rejection::Unavailable)
        }
    }
}
