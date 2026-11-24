//! 접수 순서: 어댑터가 인증·정규화한 입력 → 입구 확인 → 한도 → 멤버 확인 → 트리거 선택 → 내구 접수.
//! 어댑터는 `Admission`을 받은 뒤에만 ack한다. 접수 성공은 저장소 커밋 성공을 뜻한다.
//! 민감(private) 트리거는 일반 접수·큐에 쓰기 전에 분기하고, 전용 입구가 실패해도 일반 대화로 넘기지 않는다.
use crate::{
    active::{ActiveCheck, Release},
    entrance::{Owner, Real},
    host::{CliAccount, HostConfig, HostError, resolve_uid},
    inbound::{Event, Inbound, Respond, kind_name},
    limits::{Limits, RateLimiter},
    observe::{self, Intake},
    routes::{Active, BoundTrigger, Route, Routes, RoutesError},
    trigger,
};
use bot_core::{
    command::{Accept, Envelope, HttpRoute, JsonPayload, Target},
    id::{PrincipalId, RunId},
    intake::{PrivateCall, PrivateIntake, RequestKey},
    port::{Error, RunStore},
    profile::{ProfileCatalog, ResponseMode, Snapshot, TriggerSource, TriggerTarget},
    trace::{NoopTracer, Tracer, now_nano},
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

/// 접수 결과. 어댑터가 ack·응답을 정한다.
#[derive(Debug, Eq, PartialEq)]
pub enum Admission {
    /// 접수·run·큐·감사 커밋이 끝났다(같은 키·같은 내용의 재전달이면 기존 run).
    Accepted {
        run: RunId,
        respond: Respond,
    },
    /// 민감 입력을 전용 도구 입구가 안전하게 받았다. 큐에는 아무것도 쓰지 않았다.
    Private,
    /// 등록된 입구가 아니다. 일도 응답도 없이 받았다는 표시만 한다.
    Ignored,
    Rejected(Rejection),
}

#[derive(Debug, Eq, PartialEq)]
pub enum Rejection {
    /// 인증은 됐지만 이 프로필의 멤버가 아니다.
    Unregistered,
    /// 멤버지만 이 일을 시작할 권한이 없다.
    Forbidden,
    /// 일시적이다. 기다리면 같은 입력이 접수될 수 있다(HTTP 429 + Retry-After, Slack은 ack하지 않고 재전달).
    RateLimited(Duration),
    TooLarge,
    /// HTTP: method·path에 맞는 트리거가 없다.
    NoRoute,
    /// 본문·입력 매핑이 맞지 않는다.
    Invalid,
    /// 같은 키에 다른 내용이 왔다.
    Conflict,
    /// 저장소를 쓸 수 없다. ack하지 않아야 상대가 다시 보낸다.
    Unavailable,
}

impl Admission {
    /// 재전달을 기다리지 않고 «받았다»고 ack해도 되는가. 다시 보내도 결론이 달라지지 않는 경우만 true.
    /// 일시적인 거절(저장소 장애·속도 제한)은 false다 — 상대가 나중에 같은 입력을 다시 보내게 둔다.
    pub fn settled(&self) -> bool {
        !matches!(
            self,
            Admission::Rejected(Rejection::Unavailable | Rejection::RateLimited(_))
        )
    }
}

type UidResolver = dyn Fn(&CliAccount) -> Result<u32, HostError> + Send + Sync;

pub struct Ingress {
    catalog: Arc<dyn ProfileCatalog>,
    host: Arc<HostConfig>,
    store: Arc<dyn RunStore>,
    private: Arc<dyn PrivateIntake>,
    limits: Limits,
    entrance_limiter: RateLimiter,
    principal_limiter: RateLimiter,
    uid_of: Box<UidResolver>,
    routes: Mutex<Cached>,
    tracer: Arc<dyn Tracer>,
    active: Option<Arc<dyn ActiveCheck>>,
}

#[derive(Default)]
struct Cached {
    current: Option<Arc<Routes>>,
    /// 마지막으로 만들지 못한 활성 집합과 그 오류. 같은 집합은 요청마다 다시 만들지 않는다.
    failed: Option<(Active, RoutesError)>,
}

/// 접수 중에 알게 된 비민감 사실(스팬용)
#[derive(Default)]
struct Seen {
    trigger: Option<String>,
    private: bool,
    /// 요청이 맞은 **등록된 트리거 정의**의 (method, path). 요청의 원래 값이 아니다 — 맞지 않으면 없다(스팬에는 고정 값이 나간다).
    route: Option<(String, String)>,
}

/// 트리거 또는 기본 대화가 정한 실행 대상과 응답 방식
struct Plan {
    target: Target,
    respond: Respond,
}

impl Ingress {
    pub fn new(
        catalog: Arc<dyn ProfileCatalog>,
        host: Arc<HostConfig>,
        store: Arc<dyn RunStore>,
        private: Arc<dyn PrivateIntake>,
    ) -> Self {
        let limits = host.limits();
        Self {
            catalog,
            host,
            store,
            private,
            entrance_limiter: RateLimiter::new(limits.per_entrance_per_min),
            principal_limiter: RateLimiter::new(limits.per_principal_per_min),
            limits,
            uid_of: Box::new(resolve_uid),
            routes: Mutex::new(Cached::default()),
            tracer: Arc::new(NoopTracer),
            active: None,
        }
    }

    /// 이 프로세스의 판본 집합을 저장소의 활성 집합과 견주는 확인을 붙인다. 붙이면 구판 프로세스는 새 접수·결과
    /// 반환·승인 결정을 일시 실패로 닫는다(생략하면 확인하지 않는다 — 단독 시험용).
    pub fn with_active(mut self, active: Arc<dyn ActiveCheck>) -> Self {
        self.active = Some(active);
        self
    }

    /// 계측 수신처를 정한다(생략하면 아무것도 하지 않는다). 접수·회신·승인 스팬이 여기로 간다.
    pub fn with_tracer(mut self, tracer: Arc<dyn Tracer>) -> Self {
        self.tracer = tracer;
        self
    }

    pub fn tracer(&self) -> &Arc<dyn Tracer> {
        &self.tracer
    }

    /// 시험용: OS 사용자 해석을 바꾼다.
    pub fn with_uid_resolver(
        mut self,
        resolver: impl Fn(&CliAccount) -> Result<u32, HostError> + Send + Sync + 'static,
    ) -> Self {
        self.uid_of = Box::new(resolver);
        self
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn host(&self) -> &Arc<HostConfig> {
        &self.host
    }

    /// 활성 집합이 바뀌었으면 라우팅 표를 다시 만든다. 새 집합이 검사를 통과하지 못하면 이전 표를 계속 쓴다
    /// (처음부터 만들 수 없으면 Err). 활성 집합은 잠금 안에서 읽어 오래된 집합이 최신 표를 덮지 못하게 하고,
    /// 이미 실패한 같은 집합은 다시 만들지 않는다.
    pub fn routes(&self) -> Result<Arc<Routes>, RoutesError> {
        let mut cache = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        let active = self.catalog.active();
        if let Some(current) = &cache.current
            && current.is_current(&active)
        {
            return Ok(current.clone());
        }
        if let Some((failed, error)) = &cache.failed
            && Arc::ptr_eq(failed, &active)
        {
            return cache.current.clone().ok_or_else(|| error.clone());
        }
        match Routes::build(active.clone(), &self.host, &*self.uid_of) {
            Ok(built) => {
                let built = Arc::new(built);
                cache.current = Some(built.clone());
                cache.failed = None;
                Ok(built)
            }
            Err(error) => {
                tracing::error!(%error, "활성 프로필 집합으로 입구 표를 만들지 못했어요 — 이전 표를 계속 써요");
                cache.failed = Some((active, error.clone()));
                cache.current.clone().ok_or(error)
            }
        }
    }

    pub async fn admit(&self, inbound: Inbound) -> Admission {
        let Ok(routes) = self.routes() else {
            return Admission::Rejected(Rejection::Unavailable);
        };
        self.admit_with(&routes, inbound).await
    }

    /// 이 프로세스가 쓰는 판본 집합이 저장소의 활성 집합과 같은가. 다르면 `Stale`, 읽지 못하면 `Unavailable`.
    pub async fn verify_active(&self) -> Result<(), Error> {
        match &self.active {
            Some(check) => {
                let verified = check.verify().await;
                if let Err(error) = &verified {
                    tracing::warn!(
                        ?error,
                        "이 프로세스의 판본이 저장소의 활성 판본과 같은지 확인하지 못해 닫아요 — 새 판본이면 다시 시작하세요"
                    );
                }
                verified
            }
            None => Ok(()),
        }
    }

    /// 결과·상태를 돌려주기 직전 확인: 지금의 활성 집합에서도 멤버이고(접수 뒤 멤버 철회·입구 소유자 변경이
    /// 응답에 새어 나가지 않게) 이 프로세스의 판본이 저장소의 활성 판본인가(구판 프로세스는 로컬 표만 보므로 닫는다).
    pub async fn release(&self, real: &Real, owner: &Owner, principal: &PrincipalId) -> Release {
        if !self.still_member(real, owner, principal) {
            return Release::Forbidden;
        }
        match self.verify_active().await {
            Ok(()) => Release::Allowed,
            Err(_) => Release::Unavailable,
        }
    }

    /// 지금의 활성 집합에서도 이 주체가 같은 입구·바인딩의 멤버인가(이 프로세스의 로컬 표 기준).
    pub fn still_member(&self, real: &Real, owner: &Owner, principal: &PrincipalId) -> bool {
        let Ok(routes) = self.routes() else {
            return false;
        };
        routes.find(real).is_some_and(|route| {
            route.snapshot.id == owner.profile
                && *route.binding_id == owner.binding
                && route.snapshot.members.contains_key(principal)
        })
    }

    /// 이미 고른 라우팅 표로 접수한다. 인증과 접수가 같은 표를 쓰게 해, 그 사이에 입구 소유자가 바뀌어도
    /// 인증한 프로필과 다른 프로필로 접수되지 않는다.
    pub async fn admit_with(&self, routes: &Routes, inbound: Inbound) -> Admission {
        let started = now_nano();
        let Some(route) = routes.find(&inbound.real) else {
            return Admission::Ignored;
        };
        // 스팬에 쓸 비민감 정보를 inbound가 옮겨 가기 전에 챙긴다(본문·첨부는 싣지 않는다)
        let profile = route.snapshot.id.clone();
        let binding = route.binding_id.0.clone();
        let request = inbound.request.0.clone();
        let principal = inbound.principal.0.clone();
        let conversation = kind_name(&inbound.conversation.kind);
        let event = inbound.event.clone();
        let adapter = match inbound.real {
            Real::Cli { .. } => "cli",
            Real::SlackChannel { .. } | Real::SlackDm { .. } => "slack",
            Real::Http { .. } => "http",
        };
        let mut seen = Seen::default();
        let admission = self.admit_inner(routes, inbound, &mut seen).await;
        observe::record_intake(
            &*self.tracer,
            &Intake {
                profile: &profile,
                binding: &binding,
                request: &request,
                principal: &principal,
                conversation,
                trigger: seen.trigger.as_deref(),
                private: seen.private,
                route: seen.route.as_ref().map(|(m, p)| (m.as_str(), p.as_str())),
                adapter,
                event: &event,
                started,
            },
            &admission,
        );
        admission
    }

    async fn admit_inner(&self, routes: &Routes, inbound: Inbound, seen: &mut Seen) -> Admission {
        let Some(route) = routes.find(&inbound.real) else {
            return Admission::Ignored;
        };
        // 구판 프로세스는 새 접수를 하지 않는다(한도·권한 검사 전에 닫아 상대가 다시 보내게 한다)
        if self.verify_active().await.is_err() {
            return Admission::Rejected(Rejection::Unavailable);
        }
        let snapshot = route.snapshot;
        let profile = &snapshot.id;
        let body_text = inbound.body.to_string();
        if body_text.len() > self.limits.body_bytes
            || inbound.attachments.len() > self.limits.attachments
        {
            return Admission::Rejected(Rejection::TooLarge);
        }
        // 입구 한도는 멤버 확인 전에 센다(미등록 시도도 입구의 부하다). 키는 등록된 바인딩뿐이라 상태가 늘지 않는다.
        let entrance_key = format!("{}/{}", profile.0, route.binding_id.0);
        if let Err(wait) = self.entrance_limiter.check(&entrance_key) {
            return Admission::Rejected(Rejection::RateLimited(wait));
        }
        let Some(roles) = snapshot.members.get(&inbound.principal) else {
            tracing::debug!(profile = %profile.0, binding = %route.binding_id.0, "미등록 주체를 거절했어요");
            return Admission::Rejected(Rejection::Unregistered);
        };
        let principal_key = format!("{}/{}", profile.0, inbound.principal.0);
        if let Err(wait) = self.principal_limiter.check(&principal_key) {
            return Admission::Rejected(Rejection::RateLimited(wait));
        }
        let scopes = requester_scopes(snapshot, roles);
        let envelope = trigger::envelope_json(route.binding_id, &inbound);
        let chosen = trigger::select(route.triggers, route.binding_id, &inbound.event, &envelope);
        if let Some(bound) = chosen {
            seen.trigger = Some(bound.trigger.id.clone());
            seen.private = bound.trigger.private;
            if let TriggerSource::Http { method, path, .. } = &bound.trigger.on {
                seen.route = Some((method.clone(), path.clone()));
            }
        }
        let key = RequestKey {
            profile: profile.clone(),
            binding: route.binding_id.clone(),
            principal: inbound.principal.clone(),
            request: inbound.request.clone(),
        };
        let plan = match chosen {
            Some(bound) if bound.trigger.private => {
                return self.private(&route, bound, key, &inbound, &envelope).await;
            }
            Some(bound) => match plan_of(bound, &envelope) {
                Some(plan) => plan,
                None => return Admission::Rejected(Rejection::Invalid),
            },
            None => match &inbound.event {
                Event::Http { .. } => return Admission::Rejected(Rejection::NoRoute),
                Event::Message => {
                    let text = inbound
                        .body
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty());
                    let Some(text) = text else {
                        return Admission::Rejected(Rejection::Invalid);
                    };
                    Plan {
                        target: Target::Agent {
                            agent: route.binding.agent.clone(),
                            instruction: text.to_owned(),
                        },
                        respond: Respond::Accepted,
                    }
                }
            },
        };
        self.accept(&route, key, inbound, body_text, plan, scopes)
            .await
    }

    async fn accept(
        &self,
        route: &Route<'_>,
        key: RequestKey,
        inbound: Inbound,
        body_text: String,
        plan: Plan,
        scopes: BTreeSet<String>,
    ) -> Admission {
        // 에이전트 실행은 `run` 스코프가 있어야 시작한다. 도구 직접 호출의 스코프는 브로커가 도구마다 확인한다.
        if matches!(plan.target, Target::Agent { .. }) && !scopes.contains("run") {
            return Admission::Rejected(Rejection::Forbidden);
        }
        // HTTP 입구만 Some: 실제 라우팅에서 비교한 대문자 method와 입구 아래 경로(쿼리 제외). 본문에서 뽑지 않는다.
        let http = match &inbound.event {
            Event::Http { method, path } => Some(HttpRoute {
                method: method.clone(),
                path: path.clone(),
                response: plan.respond.into(),
            }),
            Event::Message => None,
        };
        let command = Accept {
            approver: approver_of(&inbound.principal),
            revision: route.snapshot.revision.clone(),
            target: plan.target,
            scopes,
            event: Envelope {
                profile: key.profile,
                request: key.request,
                binding: key.binding,
                principal: key.principal,
                conversation: inbound.conversation,
                http,
                attachments: inbound.attachments,
                body: JsonPayload(body_text),
            },
        };
        match self.store.accept(command).await {
            Ok(run) => Admission::Accepted {
                run,
                respond: plan.respond,
            },
            Err(error) => Admission::Rejected(rejection_of(error)),
        }
    }

    async fn private(
        &self,
        route: &Route<'_>,
        bound: &BoundTrigger,
        key: RequestKey,
        inbound: &Inbound,
        envelope: &Value,
    ) -> Admission {
        let TriggerTarget::Tool { tool, .. } = &bound.trigger.target else {
            return Admission::Rejected(Rejection::Invalid);
        };
        let Some(args) = trigger::tool_args(&bound.trigger, envelope) else {
            return Admission::Rejected(Rejection::Invalid);
        };
        let call = PrivateCall {
            key,
            revision: route.snapshot.revision.clone(),
            conversation: inbound.conversation.clone(),
            agent: bound.agent.clone(),
            tool: tool.clone(),
            args: JsonPayload(args),
        };
        match self.private.receive(call).await {
            Ok(()) => Admission::Private,
            Err(error) => Admission::Rejected(private_rejection(error)),
        }
    }
}

fn plan_of(bound: &BoundTrigger, envelope: &Value) -> Option<Plan> {
    let target = match &bound.trigger.target {
        TriggerTarget::Instruction(instruction) => Target::Agent {
            agent: bound.agent.clone(),
            instruction: instruction.clone(),
        },
        TriggerTarget::Tool { tool, .. } => Target::Tool {
            agent: bound.agent.clone(),
            tool: tool.clone(),
            args: JsonPayload(trigger::tool_args(&bound.trigger, envelope)?),
        },
    };
    let respond = match &bound.trigger.on {
        TriggerSource::Http {
            response: ResponseMode::Sync,
            ..
        } => Respond::Sync,
        _ => Respond::Accepted,
    };
    Some(Plan { target, respond })
}

/// 요청자 권한 스냅샷 = 멤버의 역할이 가진 스코프의 합집합 ∩ 프로필 정책 상한
fn requester_scopes(snapshot: &Snapshot, roles: &BTreeSet<String>) -> BTreeSet<String> {
    roles
        .iter()
        .filter_map(|role| snapshot.roles.get(role))
        .flatten()
        .filter(|scope| snapshot.policy.scopes.contains(*scope))
        .cloned()
        .collect()
}

/// 기본 승인자는 요청자다. 서비스 주체(HTTP 토큰)는 승인할 수 없으니 지정하지 않는다.
fn approver_of(principal: &PrincipalId) -> Option<PrincipalId> {
    principal.is_human().then(|| principal.clone())
}

/// 민감 입력은 영구 거절(형식·권한)만 확정으로 보고, 나머지(진행 중·시작 전 재시도·충돌·장애)는 모두 재전달 신호다.
/// 원문을 저장하지 않으므로 ack하지 않고 상대가 다시 보내게 하는 것이 안전한 쪽이다.
fn private_rejection(error: Error) -> Rejection {
    match error {
        Error::Invalid => Rejection::Invalid,
        Error::Denied => Rejection::Forbidden,
        Error::Conflict | Error::Stale | Error::Missing | Error::Unavailable => {
            Rejection::Unavailable
        }
    }
}

fn rejection_of(error: Error) -> Rejection {
    match error {
        Error::Invalid => Rejection::Invalid,
        Error::Denied => Rejection::Forbidden,
        Error::Conflict => Rejection::Conflict,
        Error::Stale | Error::Missing | Error::Unavailable => Rejection::Unavailable,
    }
}

#[cfg(test)]
mod tests;
