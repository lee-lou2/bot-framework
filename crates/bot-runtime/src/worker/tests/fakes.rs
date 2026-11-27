//! Fake ports, scripted model runners, and fixtures shared across worker unit tests.

use super::{fake_ledger::FakeLedger, fake_runs::FakeRuns};
use crate::{
    host::{Deps, Models, NoRelays, Relays, SingleModel, WorkerConfig},
    model::{ModelError, ModelRequest, ModelRunner, ModelStep, RunnerKind, StepFuture},
    worker::Worker,
};
use bot_broker::testing::tool;
use bot_core::{
    command::*,
    id::*,
    port::{Broker, DelegationStore, Error, Gateway, MemoryStore, Reply},
    profile::{Agent, Document, Policy, ProfileCatalog, Skill, Snapshot},
    state::{Conversation, ConversationKind, Risk, RunContext},
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
fn agent(parent: Option<&str>, skills: bool) -> Agent {
    let mut agent = Agent {
        description: String::new(),
        parent: parent.map(|p| AgentId(p.into())),
        model: "assistant".into(),
        instructions: "Reply concisely.".into(),
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
                description: None,
                instructions: Document {
                    relative_path: "skills/agenda/SKILL.md".into(),
                    text: "Handles schedule operations.".into(),
                },
                tools: BTreeMap::from([(
                    ToolId("agenda.list".into()),
                    tool(Risk::Read, &["calendar.read"]),
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
        roles: BTreeMap::from([(
            "owner".to_owned(),
            scopes(&["run", "calendar.read", "calendar.delete"]),
        )]),
        members: BTreeMap::from([(
            PrincipalId("local:owner".into()),
            BTreeSet::from(["owner".to_owned()]),
        )]),
        super_admins: BTreeSet::new(),
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
            input: JsonPayload(json!({"text": "Show today's agenda"}).to_string()),
            attachments: vec![],
            steps: vec![],
            pass_pos: None,
        },
        approver: None,
        trigger: None,
        origin: bot_core::state::RunOrigin::Message,
    }
}

pub(super) fn agent_run(run: &str) -> RunContext {
    base(
        run,
        1,
        Target::Agent {
            agent: AgentId("main".into()),
            instruction: "Show today's agenda".into(),
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
    fn call_for_model(&self, call: ToolCall) -> Reply<'_, bot_core::port::ModelCall> {
        Box::pin(async move {
            self.call(call)
                .await
                .map(bot_core::port::ModelCall::Returned)
        })
    }
    fn decide(&self, _: ApprovalId, _: PrincipalId, _: bool) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

#[derive(Default)]
pub(super) struct FakeDelegation {
    pub requests: Mutex<Vec<Delegate>>,
    pub error: Mutex<Option<Error>>,
    pub results: Mutex<BTreeMap<String, JsonPayload>>,
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
    pub auto_saved: Mutex<Vec<bot_core::memory::MemorySave>>,
    pub auto_forgotten: Mutex<Vec<MemoryId>>,
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
    fn auto_save(
        &self,
        _: Attempt,
        _: Revision,
        spec: bot_core::memory::MemorySave,
    ) -> Reply<'_, Option<bot_core::memory::MemorySaved>> {
        let n = {
            let mut saved = lock(&self.auto_saved);
            saved.push(spec);
            saved.len()
        };
        Box::pin(async move {
            Ok(Some(bot_core::memory::MemorySaved {
                id: MemoryId(format!("auto-{n}")),
                deduplicated: false,
            }))
        })
    }
    fn auto_forget(&self, _: Attempt, _: Revision, id: MemoryId) -> Reply<'_, bool> {
        lock(&self.auto_forgotten).push(id);
        Box::pin(async { Ok(true) })
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

type Script = VecDeque<Result<ModelStep, ModelError>>;

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
