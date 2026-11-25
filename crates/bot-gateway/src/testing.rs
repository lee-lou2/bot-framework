//! 시험·데모 보조: 합성 프로필 스냅샷 조립기와 메모리 가짜 저장소. 운영 빌드에는 넣지 않는다(`testing` 기능).
pub mod api;
pub mod cards;
pub mod catalog;
pub mod ports;
pub mod slack;

use bot_core::trace::{SpanRecord, Tracer};
pub use catalog::TestCatalog;

use bot_core::{
    command::{
        Accept, AcceptExtras, Beat, Failure, Finish, HttpRoute, JsonPayload, Retry, Step, Target,
        Turn,
    },
    id::*,
    intake::{PrivateCall, PrivateIntake, RequestKey, RequestState},
    pass::PassChain,
    port::{Error, ProfileSecrets, Reply, RunStore, SecretStore, SecretValue},
    profile::*,
    slack::SlackSource,
    state::{RunContext, RunState},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

/// 합성 스냅샷 조립기. 에이전트·바인딩·멤버·트리거만 채우고 나머지는 비워 둔다.
pub struct Fixture {
    snapshot: Snapshot,
}

pub fn profile(id: &str) -> Fixture {
    let main = AgentId("main".into());
    let agent = Agent {
        description: String::new(),
        builtin: BTreeSet::new(),
        parent: None,
        model: "assistant".into(),
        instructions: String::new(),
        env: BTreeMap::new(),
        scopes: BTreeSet::new(),
        skills: BTreeMap::new(),
        knowledge: Vec::new(),
        memory_seeds: Vec::new(),
        triggers: Vec::new(),
    };
    Fixture {
        snapshot: Snapshot {
            id: ProfileId(id.into()),
            revision: Revision(format!("{id}-r1")),
            policy_revision: Revision(format!("{id}-p1")),
            timezone: "UTC".into(),
            model: "assistant".into(),
            root: main.clone(),
            agents: BTreeMap::from([(main, agent)]),
            bindings: BTreeMap::new(),
            roles: BTreeMap::new(),
            members: BTreeMap::new(),
            policy: Policy {
                scopes: BTreeSet::new(),
                agents: BTreeMap::new(),
                approve: BTreeSet::new(),
            },
            secrets: Vec::new(),
        },
    }
}

impl Fixture {
    pub fn binding(mut self, id: &str, source: Adapter, agent: &str) -> Self {
        self.snapshot.bindings.insert(
            BindingId(id.into()),
            Binding {
                agent: AgentId(agent.into()),
                source,
            },
        );
        self
    }

    pub fn member(mut self, identity: &str, roles: &[&str]) -> Self {
        self.snapshot.members.insert(
            PrincipalId(identity.into()),
            roles.iter().map(|r| r.to_string()).collect(),
        );
        self
    }

    pub fn revision(mut self, revision: &str) -> Self {
        self.snapshot.revision = Revision(revision.into());
        self
    }

    pub fn role(mut self, name: &str, scopes: &[&str]) -> Self {
        self.snapshot
            .roles
            .insert(name.into(), scopes.iter().map(|s| s.to_string()).collect());
        self
    }

    /// 에이전트를 하나 더 둔다(`main`과 같은 모양, 스코프만 다르다)
    pub fn agent(mut self, id: &str, scopes: &[&str]) -> Self {
        let mut agent = self
            .snapshot
            .agents
            .get(&self.snapshot.root)
            .expect("루트 에이전트가 있어야 해요")
            .clone();
        agent.scopes = scopes.iter().map(|s| s.to_string()).collect();
        // 트리거는 선언한 에이전트만 소유한다
        agent.triggers = Vec::new();
        self.snapshot.agents.insert(AgentId(id.into()), agent);
        self
    }

    /// 에이전트가 가진 스코프(에이전트 상한)
    pub fn agent_scopes(mut self, agent: &str, scopes: &[&str]) -> Self {
        self.snapshot
            .agents
            .get_mut(&AgentId(agent.into()))
            .expect("에이전트가 있어야 해요")
            .scopes = scopes.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn policy_scopes(mut self, scopes: &[&str]) -> Self {
        self.snapshot.policy.scopes = scopes.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn trigger(mut self, agent: &str, trigger: Trigger) -> Self {
        self.snapshot
            .agents
            .get_mut(&AgentId(agent.into()))
            .expect("에이전트가 있어야 해요")
            .triggers
            .push(trigger);
        self
    }

    pub fn build(self) -> Arc<Snapshot> {
        Arc::new(self.snapshot)
    }
}

pub use catalog::catalog;

/// 사건 트리거(`on: {event: message, binding}`)
pub fn event_trigger(id: &str, binding: &str, target: TriggerTarget) -> Trigger {
    Trigger {
        id: id.into(),
        on: TriggerSource::Event {
            binding: BindingId(binding.into()),
            event: "message".into(),
        },
        order: 0,
        conditions: BTreeMap::new(),
        input: BTreeMap::new(),
        private: false,
        target,
        preapproved: None,
        human_approval: None,
    }
}

/// 다른 사건 이름(`thread-reply`·`reaction`)을 받는 사건 트리거
pub fn event_trigger_on(id: &str, binding: &str, event: &str, target: TriggerTarget) -> Trigger {
    let mut trigger = event_trigger(id, binding, target);
    if let TriggerSource::Event { event: name, .. } = &mut trigger.on {
        *name = event.into();
    }
    trigger
}

/// HTTP 트리거(`on: {http: {method, path}, binding}`)
pub fn http_trigger(
    id: &str,
    binding: &str,
    method: &str,
    path: &str,
    target: TriggerTarget,
) -> Trigger {
    Trigger {
        id: id.into(),
        on: TriggerSource::Http {
            binding: BindingId(binding.into()),
            method: method.into(),
            path: path.into(),
            response: ResponseMode::Accepted,
        },
        order: 0,
        conditions: BTreeMap::new(),
        input: BTreeMap::new(),
        private: false,
        target,
        preapproved: None,
        human_approval: None,
    }
}

/// HTTP 트리거의 응답 방식을 `sync`로 바꾼다.
pub fn sync(mut trigger: Trigger) -> Trigger {
    if let TriggerSource::Http { response, .. } = &mut trigger.on {
        *response = ResponseMode::Sync;
    }
    trigger
}

pub fn tool_target(tool: &str, args: &str) -> TriggerTarget {
    TriggerTarget::Tool {
        tool: ToolId(tool.into()),
        args: JsonPayload(args.into()),
    }
}

/// 접수된 요청 한 건(원문 확인용)
#[derive(Clone, Debug)]
pub struct Accepted {
    pub run: RunId,
    pub key: RequestKey,
    pub body: String,
    pub target: String,
    pub approver: Option<String>,
    pub scopes: Vec<String>,
    pub revision: String,
    /// `종류:id:thread`
    pub conversation: String,
    /// HTTP 입구면 `METHOD /path`
    pub http: Option<String>,
    /// 접수 때 저장한 HTTP 경로·응답 방식(조회가 돌려준다)
    pub route: Option<HttpRoute>,
    /// 어댑터가 덧붙인 인증된 Slack 이벤트 원천
    pub source: Option<SlackSource>,
    /// 접수가 남긴 넘김 후보 사슬
    pub chain: Option<PassChain>,
}

/// 같은 키·같은 내용은 기존 run, 다른 내용은 충돌인 메모리 저장소. 접수 순서와 결과를 시험이 직접 만진다.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// 키 → (입력 지문, 접수 기록)
    accepted: BTreeMap<String, (String, Accepted)>,
    states: BTreeMap<String, (RunState, Option<JsonPayload>)>,
    fail_next: Option<Error>,
}

fn key_text(key: &RequestKey) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        key.profile.0, key.binding.0, key.principal.0, key.request.0
    )
}

fn target_text(target: &Target) -> String {
    match target {
        Target::Agent { agent, instruction } => format!("agent {} {instruction}", agent.0),
        Target::Tool { agent, tool, args } => format!("tool {} {} {}", agent.0, tool.0, args.0),
    }
}

impl MemoryStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn accepted(&self) -> Vec<Accepted> {
        let inner = self.inner.lock().unwrap();
        let mut all: Vec<Accepted> = inner.accepted.values().map(|(_, a)| a.clone()).collect();
        all.sort_by(|a, b| a.run.0.cmp(&b.run.0));
        all
    }

    /// 이 요청의 저장된 HTTP 경로·응답 방식을 지운다(이 메타가 생기기 전의 옛 행을 흉내 낸다).
    pub fn forget_route(&self, key: &RequestKey) {
        if let Some((_, accepted)) = self.inner.lock().unwrap().accepted.get_mut(&key_text(key)) {
            accepted.route = None;
        }
    }

    /// 다음 접수 한 번을 이 오류로 실패시킨다.
    pub fn fail_next(&self, error: Error) {
        self.inner.lock().unwrap().fail_next = Some(error);
    }

    /// run의 상태·결과를 정한다(워커가 끝낸 것처럼).
    pub fn set_state(&self, key: &RequestKey, state: RunState, result: Option<&str>) {
        self.inner.lock().unwrap().states.insert(
            key_text(key),
            (state, result.map(|r| JsonPayload(r.to_string()))),
        );
    }
}

impl RunStore for MemoryStore {
    fn accept(&self, command: Accept) -> Reply<'_, RunId> {
        self.accept_with(command, AcceptExtras::default())
    }

    fn accept_with(&self, command: Accept, extras: AcceptExtras) -> Reply<'_, RunId> {
        let key = RequestKey {
            profile: command.event.profile.clone(),
            binding: command.event.binding.clone(),
            principal: command.event.principal.clone(),
            request: command.event.request.clone(),
        };
        let route = command
            .event
            .http
            .as_ref()
            .map(|r| format!("{} {}", r.method, r.path));
        let fingerprint = format!(
            "{}\u{1e}{}\u{1e}{}",
            command.event.body.0,
            target_text(&command.target),
            route.clone().unwrap_or_default()
        );
        let record = Accepted {
            run: RunId(String::new()),
            key: key.clone(),
            body: command.event.body.0.clone(),
            target: target_text(&command.target),
            approver: command.approver.as_ref().map(|a| a.0.clone()),
            scopes: command.scopes.iter().cloned().collect(),
            revision: command.revision.0.clone(),
            conversation: format!(
                "{}:{}:{}",
                crate::inbound::kind_name(&command.event.conversation.kind),
                command.event.conversation.id,
                command
                    .event
                    .conversation
                    .thread
                    .clone()
                    .unwrap_or_default()
            ),
            http: route.clone(),
            route: command.event.http.clone(),
            source: extras.source,
            chain: extras.chain,
        };
        let mut inner = self.inner.lock().unwrap();
        let result = if let Some(error) = inner.fail_next.take() {
            Err(error)
        } else if let Some((seen, first)) = inner.accepted.get(&key_text(&key)) {
            if *seen == fingerprint {
                Ok(first.run.clone())
            } else {
                Err(Error::Conflict)
            }
        } else {
            let run = RunId(format!("run-{}", inner.accepted.len() + 1));
            let text = key_text(&key);
            inner.accepted.insert(
                text.clone(),
                (
                    fingerprint,
                    Accepted {
                        run: run.clone(),
                        ..record
                    },
                ),
            );
            inner.states.insert(text, (RunState::Ready, None));
            Ok(run)
        };
        Box::pin(std::future::ready(result))
    }

    fn status(&self, key: RequestKey) -> Reply<'_, Option<RequestState>> {
        let inner = self.inner.lock().unwrap();
        let text = key_text(&key);
        let state = inner.accepted.get(&text).map(|(_, accepted)| {
            let (state, result) = inner
                .states
                .get(&text)
                .cloned()
                .unwrap_or((RunState::Ready, None));
            RequestState {
                run: accepted.run.clone(),
                state,
                result,
                http: accepted.route.clone(),
            }
        });
        Box::pin(std::future::ready(Ok(state)))
    }

    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        Box::pin(std::future::ready(Ok(None)))
    }

    fn renew(&self, _: Attempt) -> Reply<'_, Beat> {
        unavailable()
    }

    fn checkpoint_batch(&self, _: Attempt, _: Vec<Step>) -> Reply<'_, ()> {
        unavailable()
    }

    fn finish(&self, _: Attempt, _: Finish) -> Reply<'_, ()> {
        unavailable()
    }

    fn fail(&self, _: Attempt, _: Failure) -> Reply<'_, Retry> {
        unavailable()
    }

    fn history(&self, _: Attempt, _: u32) -> Reply<'_, Vec<Turn>> {
        unavailable()
    }
}

/// 게이트웨이가 쓰지 않는 포트 메서드
fn unavailable<T: Send + 'static>() -> Reply<'static, T> {
    Box::pin(std::future::ready(Err(Error::Unavailable)))
}

/// 민감 DM 전용 입구를 흉내 낸다: 원문을 이 안에만 두고 같은 키의 재전달은 한 번만 센다.
#[derive(Default)]
pub struct MemoryPrivate {
    inner: Mutex<PrivateInner>,
}

#[derive(Default)]
struct PrivateInner {
    received: BTreeMap<String, (String, String, String)>,
    fail_next: Option<Error>,
}

impl MemoryPrivate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// (도구, 인자 원문) 목록. 같은 키 재전달은 한 건이다.
    pub fn received(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .unwrap()
            .received
            .values()
            .map(|(tool, args, _)| (tool.clone(), args.clone()))
            .collect()
    }

    pub fn fail_next(&self, error: Error) {
        self.inner.lock().unwrap().fail_next = Some(error);
    }
}

impl PrivateIntake for MemoryPrivate {
    fn receive(&self, call: PrivateCall) -> Reply<'_, ()> {
        let mut inner = self.inner.lock().unwrap();
        let result = if let Some(error) = inner.fail_next.take() {
            Err(error)
        } else {
            inner.received.entry(key_text(&call.key)).or_insert((
                call.tool.0,
                call.args.0,
                call.agent.0,
            ));
            Ok(())
        };
        Box::pin(std::future::ready(result))
    }
}

/// 프로필별 이름 공간만 보는 메모리 SecretStore(시험용). 값은 Debug로 찍히지 않는다.
pub struct MemorySecrets {
    values: Mutex<BTreeMap<(String, String), Vec<u8>>>,
}

impl MemorySecrets {
    pub fn new(entries: &[(&str, &str, &[u8])]) -> Arc<Self> {
        Arc::new(Self {
            values: Mutex::new(
                entries
                    .iter()
                    .map(|(profile, name, value)| {
                        ((profile.to_string(), name.to_string()), value.to_vec())
                    })
                    .collect(),
            ),
        })
    }

    /// 값을 지운다(운영자가 보관소에서 자격을 없앤 것을 흉내 낸다: 이후 `resolve`는 `Missing`이다).
    pub fn remove(&self, profile: &str, name: &str) {
        self.values
            .lock()
            .unwrap()
            .remove(&(profile.to_owned(), name.to_owned()));
    }

    /// 값을 바꾼다(운영자가 보관소의 토큰을 교체한 것을 흉내 낸다).
    pub fn set(&self, profile: &str, name: &str, value: &[u8]) {
        self.values
            .lock()
            .unwrap()
            .insert((profile.to_owned(), name.to_owned()), value.to_vec());
    }
}

struct Namespace(BTreeMap<String, Vec<u8>>);

impl ProfileSecrets for Namespace {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        let found = self.0.get(name.as_str()).map(|v| SecretValue(v.clone()));
        Box::pin(std::future::ready(found.ok_or(Error::Missing)))
    }
}

impl SecretStore for MemorySecrets {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let own: BTreeMap<String, Vec<u8>> = self
            .values
            .lock()
            .unwrap()
            .iter()
            .filter(|((p, _), _)| *p == profile.0)
            .map(|((_, name), value)| (name.clone(), value.clone()))
            .collect();
        Box::pin(std::future::ready(Ok(
            Box::new(Namespace(own)) as Box<dyn ProfileSecrets>
        )))
    }
}

/// 끝난 스팬을 모아 두는 계측 수신처(시험용)
#[derive(Default)]
pub struct RecordingTracer {
    spans: Mutex<Vec<SpanRecord>>,
}

impl RecordingTracer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn spans(&self) -> Vec<SpanRecord> {
        self.spans.lock().unwrap().clone()
    }

    /// 이름이 `name`인 스팬들
    pub fn named(&self, name: &str) -> Vec<SpanRecord> {
        self.spans()
            .into_iter()
            .filter(|s| s.name == name)
            .collect()
    }

    /// 이름이 `name`인 스팬이 `count`개 이상 기록될 때까지(최대 5초) 기다린다. 스팬은 응답을 보낸 **뒤에** 기록될 수 있어, 응답이 도착했다고 바로 세면 흔들린다.
    pub async fn until_named(&self, name: &str, count: usize) -> bool {
        for _ in 0..500 {
            if self.named(name).len() >= count {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        self.named(name).len() >= count
    }
}

impl Tracer for RecordingTracer {
    fn record(&self, span: SpanRecord) {
        self.spans.lock().unwrap().push(span);
    }
}
