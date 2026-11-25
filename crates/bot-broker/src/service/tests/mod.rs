//! 수락 시험: 가짜 원장·카탈로그·실행기 위에서 `Broker`가 인가·승인·복구 규칙을 지키는지 본다.

mod approvals;
mod authority;
mod basics;
mod deliver;
mod deliver_commit;
mod destination;
mod env;
mod ledger;
mod legacy;
mod limited;
mod memory;
mod openai;
mod owner_precedence;
mod preflight;
mod recovery;
mod revision;
mod schedule;
mod secret_check;
mod unrouted;
mod wake;

use super::{BrokerService, Config, Parts, preflight::noting_limit};
use crate::{ports::Sealer, rate::RateConfig, testing::*};
use bot_core::{
    command::*,
    id::*,
    port::{Broker, Error},
    profile::{Destination, Snapshot},
    state::{Risk, RunContext},
};
use std::{sync::Arc, time::Duration};

pub(super) const ALL: [&str; 7] = [
    "run",
    "calendar.read",
    "calendar.delete",
    "calendar.write",
    "web.search",
    "mail.send",
    "chat.post",
];

/// 한 프로필(`personal`)과 한 실행(`r1`, 소유자 본인, CLI)을 갖춘 시험 세계.
pub(super) struct World {
    pub catalog: Arc<FakeCatalog>,
    pub ledger: Arc<MemLedger>,
    pub memory: Arc<MemMemory>,
    pub schedules: Arc<MemSchedules>,
    pub replies: Arc<MemReplies>,
    pub routes: Arc<MemRoutes>,
    pub runner: Arc<FakeRunner>,
    pub effects: Arc<FakeEffects>,
    pub audit: Arc<MemAudit>,
    pub tracer: Arc<RecordingTracer>,
    pub clock: Arc<ManualClock>,
    pub entropy: Arc<CountingEntropy>,
    pub broker: BrokerService,
    cfg: Config,
}

/// 기본 스냅샷 조립기: 역할·멤버·정책 스코프와 `main`·`main/research`의 도구.
pub(super) fn base(revision: &str) -> SnapshotBuilder {
    let mut explicit = tool(Risk::Write, &["mail.send"]);
    explicit.destination = Some(Destination::Explicit {
        recipient: "args.to".into(),
        content: "args.body".into(),
    });
    let mut reply = tool(Risk::Write, &["chat.post"]);
    reply.destination = Some(Destination::Reply);
    let mut search = tool(Risk::Read, &["web.search"]);
    search.external = true;
    let mut steps = tool(Risk::Write, &["calendar.write"]);
    steps.effects = true;
    snapshot("personal", revision)
        .role("owner", &ALL)
        .role("reader", &["calendar.read"])
        .role("ops", &["calendar.delete"])
        .member("local:owner", &["owner"])
        .member("local:alice", &["owner"])
        .member("service:morning", &["reader", "ops"])
        .policy_scopes(&ALL)
        .cron_trigger("main", "morning", "service:morning", Some("local:alice"))
        .tool("main", "agenda.list", tool(Risk::Read, &["calendar.read"]))
        .tool(
            "main",
            "agenda.delete",
            tool(Risk::Irreversible, &["calendar.delete"]),
        )
        .tool(
            "main",
            "agenda.update",
            tool(Risk::Write, &["calendar.write"]),
        )
        .tool("main", "agenda.sync", steps)
        .tool("main", "web.search", search.clone())
        .tool("main/research", "web.search", search)
        .tool("main", "mail.send", explicit)
        .tool("main", "chat.post", reply)
}

impl World {
    pub fn new() -> Self {
        Self::with(Config::default())
    }

    pub fn with(cfg: Config) -> Self {
        let catalog = Arc::new(FakeCatalog::new());
        catalog.publish(base("r1").build());
        let clock = Arc::new(ManualClock::new(1_000_000));
        let ledger = Arc::new(MemLedger::new(clock.clone()));
        ledger.add_run(owner_run("r1", 1));
        ledger.set_active_revision("personal", "r1");
        let mut world = Self {
            catalog,
            memory: Arc::new(MemMemory::new(ledger.clone())),
            schedules: Arc::new(MemSchedules::new(ledger.clone())),
            replies: Arc::new(MemReplies::new(ledger.clone())),
            routes: Arc::new(MemRoutes::new()),
            ledger,
            runner: Arc::new(FakeRunner::new()),
            effects: Arc::new(FakeEffects::new()),
            audit: Arc::new(MemAudit::new()),
            tracer: Arc::new(RecordingTracer::new()),
            entropy: Arc::new(CountingEntropy::default()),
            clock,
            cfg,
            broker: BrokerService::new(
                Parts {
                    catalog: Arc::new(FakeCatalog::new()),
                    memory: Arc::new(MemMemory::new(Arc::new(MemLedger::new(Arc::new(
                        ManualClock::new(0),
                    ))))),
                    ledger: Arc::new(MemLedger::new(Arc::new(ManualClock::new(0)))),
                    effects: Arc::new(FakeEffects::new()),
                    audit: Arc::new(MemAudit::new()),
                    runner: Arc::new(FakeRunner::new()),
                    sealer: Arc::new(FakeSealer),
                    tracer: Arc::new(RecordingTracer::new()),
                    clock: Arc::new(ManualClock::new(0)),
                    entropy: Arc::new(CountingEntropy::default()),
                },
                cfg,
            ),
        };
        world.broker = world.reboot();
        world
    }

    /// 같은 원장·카탈로그·실행기로 브로커를 새로 만든다(재시작). 예약·회신 저장소도 다시 붙인다.
    pub fn reboot(&self) -> BrokerService {
        self.bare()
            .with_schedules(self.schedules.clone())
            .with_replies(self.replies.clone())
            .with_routes(self.routes.clone())
    }

    /// 예약·회신 저장소를 붙이지 않은 브로커(저장소가 없을 때의 동작을 본다).
    pub fn bare(&self) -> BrokerService {
        self.bare_with(Arc::new(FakeSealer))
    }

    /// 봉인기를 갈아 끼운, 예약·회신 저장소를 붙이지 않은 브로커.
    pub fn bare_with(&self, sealer: Arc<dyn Sealer>) -> BrokerService {
        BrokerService::new(
            Parts {
                catalog: self.catalog.clone(),
                memory: self.memory.clone(),
                ledger: self.ledger.clone(),
                effects: self.effects.clone(),
                audit: self.audit.clone(),
                runner: self.runner.clone(),
                sealer,
                tracer: self.tracer.clone(),
                clock: self.clock.clone(),
                entropy: self.entropy.clone(),
            },
            self.cfg,
        )
    }

    pub fn tool_call(
        &self,
        run: &str,
        generation: u64,
        call: &str,
        tool: &str,
        args: &str,
    ) -> ToolCall {
        ToolCall {
            attempt: Attempt {
                profile: ProfileId("personal".into()),
                run: RunId(run.into()),
                claim: Some(self.ledger.claim_epoch(run)),
                generation: Generation(generation),
            },
            call: CallId(call.into()),
            tool: ToolId(tool.into()),
            args: JsonPayload(args.into()),
        }
    }

    pub fn call_on(
        &self,
        broker: &BrokerService,
        run: &str,
        generation: u64,
        call: &str,
        tool: &str,
        args: &str,
    ) -> Result<CallResult, Error> {
        block_on(broker.call(self.tool_call(run, generation, call, tool, args)))
    }

    /// `call`과 같되 속도·동시성 한도가 **이 호출을 거절했는지**(`noting_limit`)도 결과와 함께 돌려준다.
    pub fn call_noted(
        &self,
        call: &str,
        tool: &str,
        args: &str,
    ) -> (Result<CallResult, Error>, bool) {
        block_on(noting_limit(
            self.broker.call(self.tool_call("r1", 1, call, tool, args)),
        ))
    }

    pub fn call(&self, call: &str, tool: &str, args: &str) -> Result<CallResult, Error> {
        self.call_on(&self.broker, "r1", 1, call, tool, args)
    }

    pub fn decide(&self, nonce: &ApprovalId, actor: &str, allow: bool) -> Result<(), Error> {
        block_on(
            self.broker
                .decide(nonce.clone(), PrincipalId(actor.into()), allow),
        )
    }

    /// 새 판본을 게시하고 활성으로 만든다.
    pub fn publish(&self, snapshot: Arc<Snapshot>) {
        self.ledger
            .set_active_revision(&snapshot.id.0, &snapshot.revision.0);
        self.catalog.publish(snapshot);
    }

    /// 호출을 원장에 만들어 `Started`로 둔다(옛 세대가 시작만 하고 사라진 상황).
    pub fn leave_started(&self, run: &str, call: &str, tool: &str, args: &str, generation: u64) {
        let snapshot = self
            .catalog
            .current(&ProfileId("personal".into()))
            .expect("snapshot");
        let found =
            crate::resolve::find_tool(&snapshot, &AgentId("main".into()), &ToolId(tool.into()))
                .expect("tool")
                .clone();
        let sealed = FakeSealer
            .seal(&found, &JsonPayload(args.into()))
            .expect("seal");
        let attempt = Attempt {
            profile: ProfileId("personal".into()),
            run: RunId(run.into()),
            claim: Some(self.ledger.claim_epoch(run)),
            generation: Generation(generation),
        };
        use bot_core::ledger::CallLedger;
        block_on(self.ledger.begin(
            attempt,
            CallId(call.into()),
            ToolId(tool.into()),
            sealed.sha256,
        ))
        .expect("begin");
        self.ledger
            .force_started(run, call, generation, &format!("{run}.{generation}.{call}"));
    }
}

use bot_core::profile::ProfileCatalog;

pub(super) fn owner_run(run: &str, generation: u64) -> RunContext {
    run_context("personal", run, generation, "local:owner", "main", &ALL)
}

pub(super) fn awaiting(result: Result<CallResult, Error>) -> ApprovalId {
    match result {
        Ok(CallResult::Awaiting(nonce)) => nonce,
        Ok(other) => panic!("승인 대기가 아니다: {other:?}"),
        Err(error) => panic!("승인 대기가 아니다: {error:?}"),
    }
}

pub(super) fn done(result: Result<CallResult, Error>) -> String {
    match result {
        Ok(CallResult::Done(payload)) => payload.0,
        Ok(other) => panic!("끝나지 않았다: {other:?}"),
        Err(error) => panic!("끝나지 않았다: {error:?}"),
    }
}

pub(super) fn is_held(result: &Result<CallResult, Error>) -> bool {
    matches!(result, Ok(CallResult::Held(_)))
}

pub(super) fn cfg_with_rate(per_minute: u32, burst: u32, concurrent: u32) -> Config {
    Config {
        approval_ttl: Duration::from_secs(30 * 60),
        rate: RateConfig {
            per_minute,
            burst,
            concurrent,
        },
    }
}
