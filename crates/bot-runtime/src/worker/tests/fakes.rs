//! 워커 시험용 가짜 포트와 고정 입력.
use super::fake_ledger::FakeLedger;
use crate::{
    host::{Deps, Models, NoRelays, Relays, SingleModel, WorkerConfig},
    model::{ModelError, ModelRequest, ModelRunner, ModelStep, RunnerKind, StepFuture},
    worker::Worker,
};
use bot_core::{
    command::*,
    id::*,
    intake::{RequestKey, RequestState},
    port::{Broker, DelegationStore, Error, Gateway, MemoryStore, Reply, RunStore},
    profile::{Agent, Document, Policy, ProfileCatalog, Skill, Snapshot, Tool},
    state::{Conversation, ConversationKind, RunContext},
    trace::{SpanRecord, Tracer},
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

pub(super) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
pub(super) fn scopes(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn tool_decl(needs: &[&str]) -> Tool {
    Tool {
        server: "demo-agenda".into(),
        revision: Revision("t1".into()),
        class: bot_core::state::Risk::Read,
        scopes: scopes(needs),
        input: JsonPayload(r#"{"type":"object"}"#.into()),
        env: Default::default(),
        secrets: BTreeMap::new(),
        egress: BTreeSet::new(),
        effects: false,
        external: false,
        destination: None,
        definition: None,
        mcp_name: None,
        timeout_secs: None,
    }
}

fn agent(parent: Option<&str>, skills: bool) -> Agent {
    let mut agent = Agent {
        description: String::new(),
        parent: parent.map(|p| AgentId(p.into())),
        model: "assistant".into(),
        instructions: "한국어로 짧게 답한다.".into(),
        env: BTreeMap::new(),
        scopes: scopes(&["run", "calendar.read", "calendar.delete"]),
        builtin: BTreeSet::new(),
        skills: BTreeMap::new(),
        knowledge: vec![],
        memory_seeds: vec![],
        triggers: vec![],
    };
    if skills {
        agent.skills.insert(
            "agenda".into(),
            Skill {
                instructions: Document {
                    relative_path: "skills/agenda/SKILL.md".into(),
                    text: "일정을 다룬다.".into(),
                },
                tools: BTreeMap::from([(
                    ToolId("agenda.list".into()),
                    tool_decl(&["calendar.read"]),
                )]),
            },
        );
    }
    agent
}

pub(super) fn snapshot() -> Snapshot {
    Snapshot {
        id: ProfileId("personal".into()),
        revision: Revision("r1".into()),
        policy_revision: Revision("p1".into()),
        timezone: "Asia/Seoul".into(),
        model: "assistant".into(),
        root: AgentId("main".into()),
        agents: BTreeMap::from([
            (AgentId("main".into()), agent(None, true)),
            (AgentId("main/research".into()), agent(Some("main"), false)),
        ]),
        bindings: BTreeMap::new(),
        // 위임은 요청자가 지금도 멤버이고 자식 에이전트에서 `run`을 쓸 수 있을 때만 한다
        roles: BTreeMap::from([(
            "owner".to_owned(),
            scopes(&["run", "calendar.read", "calendar.delete"]),
        )]),
        members: BTreeMap::from([(
            PrincipalId("local:owner".into()),
            BTreeSet::from(["owner".to_owned()]),
        )]),
        policy: Policy {
            scopes: scopes(&["run", "calendar.read", "calendar.delete"]),
            agents: BTreeMap::new(),
            approve: BTreeSet::new(),
        },
        secrets: vec![],
    }
}

fn base(run: &str, generation: u64, target: Target, kind: ConversationKind) -> RunContext {
    RunContext {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId(run.into()),
            claim: Some(ClaimEpoch(1)),
            generation: Generation(generation),
        },
        root: RunId(run.into()),
        principal: PrincipalId("local:owner".into()),
        agent: AgentId("main".into()),
        conversation: Conversation {
            kind,
            id: "c1".into(),
            thread: None,
        },
        binding: BindingId("desk".into()),
        api: None,
        scopes: scopes(&["run", "calendar.read", "calendar.delete"]),
        private_memory_read: false,
        work: Work {
            revision: Revision("r1".into()),
            target,
            input: JsonPayload(json!({"text": "오늘 일정 알려줘"}).to_string()),
            attachments: vec![],
            steps: vec![],
            pass_pos: None,
        },
        approver: None,
        trigger: None,
        origin: bot_core::state::RunOrigin::Message,
    }
}

/// 에이전트 대상 run(CLI 대화).
pub(super) fn agent_run(run: &str) -> RunContext {
    base(
        run,
        1,
        Target::Agent {
            agent: AgentId("main".into()),
            instruction: "오늘 일정 알려줘".into(),
        },
        ConversationKind::Cli,
    )
}

pub(super) fn direct_run(run: &str, tool: &str) -> RunContext {
    base(
        run,
        1,
        Target::Tool {
            agent: AgentId("main".into()),
            tool: ToolId(tool.into()),
            args: JsonPayload("{}".into()),
        },
        ConversationKind::Scheduled,
    )
}

pub(super) fn in_kind(mut ctx: RunContext, kind: ConversationKind) -> RunContext {
    ctx.conversation.kind = kind;
    ctx
}

pub(super) fn generation(mut ctx: RunContext, generation: u64) -> RunContext {
    ctx.attempt.generation = Generation(generation);
    ctx
}

// ---- 저장소 ----

#[derive(Default)]
pub(super) struct RunsState {
    pub queue: VecDeque<RunContext>,
    pub history: Vec<Turn>,
    pub steps: Vec<Step>,
    pub finished: Vec<Finish>,
    /// `finish_with`로 받은 실행 계량(완료마다 하나)
    pub outcomes: Vec<RunOutcome>,
    pub failed: Vec<Failure>,
    pub renews: u32,
    pub renew_script: VecDeque<Result<Beat, Error>>,
    pub finish_error: Option<Error>,
    /// 다음 `history` 한 번이 돌려줄 오류(소유권을 잃은 것처럼 새 읽기가 `Stale`로 거절될 때)
    pub history_error: Option<Error>,
    /// 운영자가 취소를 요청했다: 하트비트가 이를 알리고, 실제 저장소처럼 새 읽기를 `Stale`로 거절한다(`fence`)
    pub cancel: bool,
    pub events: Vec<String>,
}

pub(super) struct FakeRuns {
    pub state: Mutex<RunsState>,
}

impl FakeRuns {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(RunsState::default()),
        })
    }
    pub(super) fn queue(&self, ctx: RunContext) {
        lock(&self.state).queue.push_back(ctx);
    }
    pub(super) fn renew_script(&self, result: Result<Beat, Error>) {
        lock(&self.state).renew_script.push_back(result);
    }
    pub(super) fn events(&self) -> Vec<String> {
        lock(&self.state).events.clone()
    }
}

impl RunStore for FakeRuns {
    fn accept_with(&self, _: Accept, _: AcceptExtras) -> Reply<'_, RunId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn status(&self, _: RequestKey) -> Reply<'_, Option<RequestState>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        let next = lock(&self.state).queue.pop_front();
        Box::pin(async move { Ok(next) })
    }
    fn renew(&self, _: Attempt) -> Reply<'_, Beat> {
        let mut state = lock(&self.state);
        state.renews += 1;
        let cancel_requested = state.cancel;
        let result = state
            .renew_script
            .pop_front()
            .unwrap_or(Ok(Beat { cancel_requested }));
        Box::pin(async move { result })
    }
    fn checkpoint_batch(&self, _: Attempt, steps: Vec<Step>) -> Reply<'_, ()> {
        let mut state = lock(&self.state);
        let mut pending = state.steps.clone();
        for step in steps {
            state.events.push(format!("checkpoint:{}", step.key));
            match pending.iter().find(|old| old.key == step.key) {
                Some(old) if old.body != step.body => {
                    return Box::pin(async { Err(Error::Conflict) });
                }
                Some(_) => {}
                None => pending.push(step),
            }
        }
        state.steps = pending;
        Box::pin(async { Ok(()) })
    }
    fn finish(&self, _: Attempt, done: Finish) -> Reply<'_, Completion> {
        let mut state = lock(&self.state);
        if state.cancel {
            return Box::pin(async { Ok(Completion::Cancelled) });
        }
        let error = state.finish_error.take();
        if error.is_none() {
            state.finished.push(done);
        }
        Box::pin(async move { error.map_or(Ok(Completion::Succeeded), Err) })
    }
    fn finish_with(&self, at: Attempt, done: Finish, outcome: RunOutcome) -> Reply<'_, Completion> {
        lock(&self.state).outcomes.push(outcome);
        self.finish(at, done)
    }
    fn fail(&self, _: Attempt, failure: Failure) -> Reply<'_, Retry> {
        lock(&self.state).failed.push(failure);
        Box::pin(async { Ok(Retry::Dead) })
    }
    fn history(&self, _: Attempt, _: u32) -> Reply<'_, Vec<Turn>> {
        let mut state = lock(&self.state);
        let result = match state.history_error.take() {
            Some(error) => Err(error),
            None if state.cancel => Err(Error::Stale),
            None => Ok(state.history.clone()),
        };
        Box::pin(async move { result })
    }
}

// ---- 브로커·위임·기억·게이트웨이 ----

pub(super) struct FakeBroker {
    pub script: Mutex<VecDeque<Result<CallResult, Error>>>,
    pub calls: Mutex<Vec<ToolCall>>,
    pub runs: Arc<FakeRuns>,
}

impl FakeBroker {
    pub(super) fn script(&self, result: Result<CallResult, Error>) {
        lock(&self.script).push_back(result);
    }
    pub(super) fn calls(&self) -> Vec<ToolCall> {
        lock(&self.calls).clone()
    }
}

pub(super) fn done(json: &str) -> Result<CallResult, Error> {
    Ok(CallResult::Done(JsonPayload(json.into())))
}

impl Broker for FakeBroker {
    fn call(&self, call: ToolCall) -> Reply<'_, CallResult> {
        lock(&self.runs.state)
            .events
            .push(format!("call:{}", call.tool.0));
        lock(&self.calls).push(call);
        let next = lock(&self.script).pop_front();
        Box::pin(async move { next.unwrap_or_else(|| done(r#"{"ok":true}"#)) })
    }
    fn decide(&self, _: ApprovalId, _: PrincipalId, _: bool) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

#[derive(Default)]
pub(super) struct FakeDelegation {
    pub requests: Mutex<Vec<Delegate>>,
    pub error: Mutex<Option<Error>>,
    /// 단계 키 → 저장소가 대조해 읽어 줄 자식 결과(`{"state":…}`). 없으면 아직 끝나지 않았다.
    pub results: Mutex<BTreeMap<String, JsonPayload>>,
    /// 결과를 읽을 때 받은 명령
    pub reads: Mutex<Vec<Delegate>>,
    pub read_error: Mutex<Option<Error>>,
}

impl DelegationStore for FakeDelegation {
    fn result(&self, command: Delegate) -> Reply<'_, Option<JsonPayload>> {
        let found = lock(&self.results).get(&command.step).cloned();
        lock(&self.reads).push(command);
        let error = lock(&self.read_error).take();
        Box::pin(async move { error.map_or(Ok(found), Err) })
    }
    fn delegate(&self, command: Delegate) -> Reply<'_, RunId> {
        lock(&self.requests).push(command);
        let error = lock(&self.error).take();
        Box::pin(async move { error.map_or(Ok(RunId("child-1".into())), Err) })
    }
}

#[derive(Default)]
pub(super) struct FakeMemory {
    pub hits: Mutex<Vec<MemoryHit>>,
    pub fail: Mutex<bool>,
}

impl MemoryStore for FakeMemory {
    fn search(&self, _: Attempt, _: String) -> Reply<'_, Vec<MemoryHit>> {
        let (hits, fail) = (lock(&self.hits).clone(), *lock(&self.fail));
        Box::pin(async move {
            if fail {
                Err(Error::Unavailable)
            } else {
                Ok(hits)
            }
        })
    }
    fn preview(&self, _: Attempt, _: MemoryId) -> Reply<'_, MemoryPreview> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn change(&self, _: Attempt, _: MemoryChange) -> Reply<'_, MemoryId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn share(&self, _: bot_core::ledger::StartCall, _: MemoryId) -> Reply<'_, MemoryId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn seed(&self, _: MemorySeed) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

#[derive(Default)]
pub(super) struct FakeGateway {
    pub delivered: Mutex<Vec<RunId>>,
}

impl Gateway for FakeGateway {
    fn deliver(&self, run: RunId, _: ProfileId) -> Reply<'_, ()> {
        lock(&self.delivered).push(run);
        Box::pin(async { Ok(()) })
    }
}

/// 실행이 고정한 판본(`pinned`)과 지금 활성인 판본(`current`)이 다를 수 있는 카탈로그. 기본은 둘이 같다.
pub(super) struct FakeCatalog {
    pub pinned: Arc<Snapshot>,
    pub current: Mutex<Arc<Snapshot>>,
}

impl FakeCatalog {
    pub(super) fn new(snapshot: Snapshot) -> Arc<Self> {
        let pinned = Arc::new(snapshot);
        Arc::new(Self {
            current: Mutex::new(pinned.clone()),
            pinned,
        })
    }
}

impl ProfileCatalog for FakeCatalog {
    fn active(&self) -> Arc<BTreeMap<ProfileId, Arc<Snapshot>>> {
        let current = lock(&self.current).clone();
        Arc::new(BTreeMap::from([(current.id.clone(), current)]))
    }
    fn lookup(&self, profile: &ProfileId, revision: &Revision) -> Option<Arc<Snapshot>> {
        (self.pinned.id == *profile && self.pinned.revision == *revision)
            .then(|| self.pinned.clone())
    }
}

#[derive(Default)]
pub(super) struct Spans(pub Mutex<Vec<SpanRecord>>);

impl Tracer for Spans {
    fn record(&self, span: SpanRecord) {
        lock(&self.0).push(span);
    }
}

// ---- 모델 ----

type Script = VecDeque<Result<ModelStep, ModelError>>;

/// 정해 둔 차례를 순서대로 내는 Step 방식 실행기. 받은 요청을 모두 기록한다.
pub(super) struct ScriptedModel {
    pub script: Mutex<Script>,
    pub requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    pub(super) fn new(steps: Vec<Result<ModelStep, ModelError>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(steps.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
    pub(super) fn requests(&self) -> Vec<ModelRequest> {
        lock(&self.requests).clone()
    }
}

impl ModelRunner for ScriptedModel {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }
    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        lock(&self.requests).push(request);
        let next = lock(&self.script).pop_front();
        Box::pin(async move {
            next.unwrap_or(Err(ModelError::Failed {
                code: "no_script",
                transient: false,
            }))
        })
    }
}

/// 취소 신호가 올 때까지 기다리는 실행기(Loop 방식: CLI처럼 한 번 불리고 오래 걸린다).
pub(super) struct BlockingModel;

impl ModelRunner for BlockingModel {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, mut request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            while !*request.cancel.borrow() {
                if request.cancel.changed().await.is_err() {
                    break;
                }
            }
            Err(ModelError::Cancelled)
        })
    }
}

pub(super) fn say(text: &str) -> Result<ModelStep, ModelError> {
    Ok(ModelStep::Final {
        text: text.into(),
        usage: crate::model::Usage {
            input_tokens: 10,
            output_tokens: 3,
        },
        outcome: Default::default(),
    })
}

pub(super) fn calls(text: &str, items: &[(&str, &str, &str)]) -> Result<ModelStep, ModelError> {
    Ok(ModelStep::Calls {
        text: text.into(),
        calls: items
            .iter()
            .map(|(call, tool, args)| crate::model::PendingCall {
                call: CallId((*call).into()),
                tool: ToolId((*tool).into()),
                args: JsonPayload((*args).into()),
            })
            .collect(),
        usage: crate::model::Usage::default(),
        measured: None,
        state: None,
    })
}

// ---- 조립 ----

pub(super) struct Rig {
    pub worker: Worker,
    pub runs: Arc<FakeRuns>,
    pub broker: Arc<FakeBroker>,
    pub delegation: Arc<FakeDelegation>,
    pub memory: Arc<FakeMemory>,
    pub gateway: Arc<FakeGateway>,
    pub ledger: Arc<FakeLedger>,
    pub catalog: Arc<FakeCatalog>,
    pub spans: Arc<Spans>,
}

pub(super) fn config() -> WorkerConfig {
    WorkerConfig {
        concurrency: 2,
        poll: Duration::from_millis(5),
        heartbeat: Duration::from_millis(10),
        heartbeat_misses: 2,
        max_turns: 6,
        history_turns: 5,
        grace: Duration::from_millis(500),
        ..WorkerConfig::default()
    }
}

pub(super) fn rig_with(model: Arc<dyn ModelRunner>, cfg: WorkerConfig) -> Rig {
    rig_models(Arc::new(SingleModel(model)), cfg)
}

pub(super) fn rig_with_relays(model: Arc<dyn ModelRunner>, relays: Arc<dyn Relays>) -> Rig {
    rig_full(Arc::new(SingleModel(model)), config(), relays)
}

pub(super) fn rig_models(models: Arc<dyn Models>, cfg: WorkerConfig) -> Rig {
    rig_full(models, cfg, Arc::new(NoRelays))
}

fn rig_full(models: Arc<dyn Models>, cfg: WorkerConfig, relays: Arc<dyn Relays>) -> Rig {
    let runs = FakeRuns::new();
    let broker = Arc::new(FakeBroker {
        script: Mutex::new(VecDeque::new()),
        calls: Mutex::new(Vec::new()),
        runs: runs.clone(),
    });
    let delegation = Arc::new(FakeDelegation::default());
    let memory = Arc::new(FakeMemory::default());
    let gateway = Arc::new(FakeGateway::default());
    let catalog = FakeCatalog::new(snapshot());
    let ledger = Arc::new(FakeLedger::default());
    *lock(&ledger.active) = Some(Revision("r1".into()));
    let spans = Arc::new(Spans::default());
    let deps = Deps {
        runs: runs.clone(),
        memory: memory.clone(),
        delegation: delegation.clone(),
        broker: broker.clone(),
        ledger: ledger.clone(),
        catalog: catalog.clone(),
        models,
        relays,
        tracer: spans.clone(),
        gateway: Some(gateway.clone()),
    };
    Rig {
        worker: Worker::new(deps, cfg),
        runs,
        broker,
        delegation,
        memory,
        gateway,
        ledger,
        catalog,
        spans,
    }
}

pub(super) fn rig(model: Arc<dyn ModelRunner>) -> Rig {
    rig_with(model, config())
}
