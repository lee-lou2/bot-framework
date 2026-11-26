//! 사전 승인 시험 도구: 실제 PG 저장소와 실제 브로커(가짜 실행기) 위에 `preapproved` cron 트리거를 올린다.
#![allow(dead_code)]

use super::*;
use bot_broker::{
    preapproval::grant_spec,
    seal::JcsSealer,
    service::{BrokerService, Config as BrokerConfig, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, RecordingTracer, snapshot, tool as fake_tool},
};
use bot_core::{
    admin::AdminAct,
    grant::{Authority, GrantCas, GrantId, GrantSpec, Granted, PauseOutcome, Preapproval},
    port::{Error, ScheduleStore},
    profile::{ActiveRevisions, InputValidator, Snapshot, TriggerTarget},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicU32},
    time::{Duration, UNIX_EPOCH},
};

pub const SERVICE: &str = "service:morning";
pub const IMAGE: &str = "reports-img-1";
pub const ARGS: &str = r#"{"report":"daily"}"#;
pub const TOOL: &str = "reports.notify";
/// 승인 ID에 묶인 도구 판본(설치 판본까지): 브로커가 `StartCall`에 싣는 값과 같다.
pub const TOOL_REVISION: &str = "t1@reports-img-1";

static SETS: AtomicU32 = AtomicU32::new(0);

struct AnyArgs;

impl InputValidator for AnyArgs {
    fn validate(&self, _: &JsonPayload, _: &JsonPayload) -> Result<(), Error> {
        Ok(())
    }
}

/// `preapproved` 선언이 있는 cron 트리거 `notify`를 가진 프로필 personal(r1).
pub fn world(max: u32, pairs: &[(&str, &str)]) -> Arc<Snapshot> {
    let mut notify = fake_tool(Risk::Irreversible, &["reports.post"]);
    notify.server = "reports".into();
    notify.effects = true;
    let mut snap = snapshot("personal", "r1")
        .role("svc", &["run", "reports.post"])
        .member(SERVICE, &["svc"])
        .member(OWNER, &["svc"])
        .policy_scopes(&["run", "reports.post"])
        .cron_trigger("main", "notify", SERVICE, None)
        .tool("main", TOOL, notify.clone())
        .tool("main/research", TOOL, notify)
        .build();
    let main = Arc::make_mut(&mut snap)
        .agents
        .get_mut(&AgentId("main".into()))
        .unwrap();
    let trigger = &mut main.triggers[0];
    trigger.target = TriggerTarget::Tool {
        tool: ToolId(TOOL.into()),
        args: JsonPayload(ARGS.into()),
    };
    trigger.preapproved = Some(Preapproval {
        max_per_day: max,
        targets: pairs
            .iter()
            .map(|(o, t)| ((*o).to_owned(), (*t).to_owned()))
            .collect(),
    });
    snap
}

pub fn run_scopes_of_trigger() -> std::collections::BTreeSet<String> {
    scopes(&["reports.post"])
}

/// 브로커가 호출 시작 때 같은 선언에서 다시 계산하는 승인 범위.
pub fn spec_of(snap: &Snapshot, image: &str) -> GrantSpec {
    let agent = AgentId("main".into());
    grant_spec(
        snap,
        &agent,
        &snap.agents[&agent].triggers[0],
        &run_scopes_of_trigger(),
        image,
        &[],
    )
    .expect("승인 범위")
}

pub fn target() -> Target {
    Target::Tool {
        agent: AgentId("main".into()),
        tool: ToolId(TOOL.into()),
        args: JsonPayload(ARGS.into()),
    }
}

/// 활성 집합을 게시한다(예약 정의와 사전 승인 포함). 돌려주는 값은 이번에 발급한 승인이다.
pub async fn publish(
    db: &TestDb,
    snap: &Snapshot,
    grants: Vec<GrantSpec>,
) -> Vec<(GrantId, GrantSpec)> {
    let expected = db.store.current().await.unwrap();
    let next = Revision(format!(
        "grant-set-{}",
        SETS.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let mut spec = trigger_spec("notify", "* * * * *", false);
    spec.scopes = run_scopes_of_trigger();
    spec.target = target();
    let profiles = BTreeMap::from([
        (ProfileId("personal".into()), snap.revision.clone()),
        (ProfileId("work".into()), Revision("r1".into())),
    ]);
    let schedules = BTreeMap::from([(ProfileId("personal".into()), vec![spec])]);
    // 게시 전 준비: 이 집합의 ID들이 고정한 판본 전부(실제 호스트는 번들을 복원해 이 목록을 만든다)
    let prepared = bot_core::profile::ProfileBundles::pinned_revisions(&db.store, &profiles)
        .await
        .unwrap();
    ActiveRevisions::replace_granted(
        &db.store,
        expected,
        next,
        profiles,
        vec![],
        schedules,
        bot_core::grant::GrantPlan { prepared, grants },
    )
    .await
    .unwrap()
}

pub struct Rig {
    pub db: TestDb,
    pub broker: BrokerService,
    pub runner: Arc<FakeRunner>,
    pub catalog: Arc<FakeCatalog>,
    pub snap: Arc<Snapshot>,
    pub spec: GrantSpec,
    pub grant: GrantId,
}

pub async fn rig(max: u32, pairs: &[(&str, &str)]) -> Option<Rig> {
    let db = TestDb::new().await?;
    let snap = world(max, pairs);
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(snap.clone());
    let runner = Arc::new(FakeRunner::new());
    runner.set_image("reports", Some(IMAGE));
    let store = Arc::new(db.store.clone());
    let broker = BrokerService::new(
        Parts {
            catalog: catalog.clone(),
            ledger: store.clone(),
            effects: store.clone(),
            memory: store.clone(),
            audit: store.clone(),
            runner: runner.clone(),
            sealer: Arc::new(JcsSealer::new(Arc::new(AnyArgs))),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        BrokerConfig::default(),
    )
    .with_schedules(store);
    let spec = spec_of(&snap, IMAGE);
    let issued = publish(&db, &snap, vec![spec.clone()]).await;
    db.backdate_schedule().await;
    let grant = issued.into_iter().next().expect("발급").0;
    Some(Rig {
        db,
        broker,
        runner,
        catalog,
        snap,
        spec,
        grant,
    })
}

pub fn at(ms: i64) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms as u64)
}

impl Rig {
    /// 다음 발생(커서 뒤 `k`분, 마지막 정지·재개 전환보다 앞서지 않는 시각)을 접수한다.
    pub async fn occur(&self, k: i64) -> Result<RunId, Error> {
        let cursor = self
            .db
            .scalar::<i64>("select cursor_ms from bot_schedule_cursors where trigger = 'notify'")
            .await;
        let changed = self
            .db
            .scalar::<i64>("select coalesce(max(changed_ms), 0) from bot_trigger_pause")
            .await;
        // 커서 뒤 `k`분(과거로 되돌려 둔 구간)이되, 전환 뒤로 밀리면 지금까지로 눌러 담는다(미래 시각은 접수하지 않는다)
        let now = self.db.now_ms().await;
        let ms = (cursor + 60_000 * k).max(changed).min(now);
        let ms = if ms > cursor {
            ms
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
            self.db.now_ms().await
        };
        self.occur_at(ms).await
    }

    /// 예정 시각이 `ms`인 발생을 접수한다(밀린 따라잡기를 흉내 낼 때).
    pub async fn occur_at(&self, ms: i64) -> Result<RunId, Error> {
        self.db
            .store
            .accept_occurrence(
                Occurrence {
                    profile: ProfileId("personal".into()),
                    agent: AgentId("main".into()),
                    trigger: "notify".into(),
                    scheduled_at: at(ms),
                },
                occurrence_accept("personal", "notify", ms, target(), &["reports.post"]),
            )
            .await
    }

    /// 발생을 접수하고 claim한다.
    pub async fn next_run(&self) -> RunContext {
        self.occur(1).await.unwrap();
        runs(&self.db).claim().await.unwrap().unwrap()
    }

    /// 발생을 접수해 claim한다. 접수가 없으면(건너뛴 발생) None이다.
    pub async fn next_run_unchecked(&self) -> Option<RunContext> {
        match self.occur(1).await {
            Ok(_) => runs(&self.db).claim().await.unwrap(),
            Err(_) => None,
        }
    }

    /// 직접 호출 기록(`{run}:tool`)을 만든다.
    pub async fn begin(&self, ctx: &RunContext, args: &str) -> CallId {
        let call = CallId(format!("{}:tool", ctx.attempt.run.0));
        calls(&self.db)
            .begin(ctx.attempt.clone(), call.clone(), tool(TOOL), sha(args))
            .await
            .unwrap();
        call
    }

    /// 브로커가 보내는 모양 그대로 사전 승인으로 시작한다.
    pub async fn start(&self, ctx: &RunContext, call: &CallId) -> Result<Granted, Error> {
        let authority = Authority::Grant {
            fingerprint: self.spec.fingerprint,
        };
        calls(&self.db)
            .start_with(start_of(ctx, call), authority)
            .await
    }

    /// 시작한 직접 호출이 있는 run(효과 요청을 받을 수 있다).
    pub async fn started(&self) -> (RunContext, CallId) {
        let ctx = self.next_run().await;
        let call = self.begin(&ctx, ARGS).await;
        assert_eq!(self.start(&ctx, &call).await.unwrap(), Granted::Started);
        (ctx, call)
    }

    pub async fn effect(
        &self,
        ctx: &RunContext,
        call: &CallId,
        step: &str,
        operation: &str,
        target: &str,
    ) -> Result<bot_core::port::EffectPermit, Error> {
        effects(&self.db)
            .request(effect_req(
                ctx,
                call,
                step,
                &format!(
                    r#"{{"operation":"{operation}","target":"{target}","params":{{"item":"{step}"}}}}"#
                ),
            ))
            .await
    }

    pub async fn pause(&self, grant: &str, epoch: i64, on: bool) -> Result<PauseOutcome, Error> {
        pause(self, grant, epoch, on).await
    }

    pub async fn used(&self) -> i64 {
        self.db
            .scalar::<i64>("select coalesce(sum(used), 0)::bigint from bot_grant_usage")
            .await
    }

    pub async fn audits(&self, action: &str) -> i64 {
        self.db
            .scalar::<i64>(&format!(
                "select count(*) from bot_audit where action = '{action}'"
            ))
            .await
    }

    pub async fn verdicts(&self, action: &str) -> Vec<String> {
        sqlx::query_scalar(&format!(
            "select verdict from bot_audit where action = '{action}' order by seq"
        ))
        .fetch_all(self.db.store.pool())
        .await
        .unwrap()
    }
}

pub fn effect_req(ctx: &RunContext, call: &CallId, step: &str, args: &str) -> EffectRequest {
    EffectRequest {
        attempt: ctx.attempt.clone(),
        call: call.clone(),
        step: step.into(),
        args: JsonPayload(args.into()),
    }
}

pub fn sha(args: &str) -> [u8; 32] {
    let value = bot_core::json::parse(args).expect("json");
    bot_core::canon::sha256(&bot_core::canon::canonicalize(&value).expect("canonical"))
}

pub fn start_of(ctx: &RunContext, call: &CallId) -> StartCall {
    StartCall {
        checked_revision: Revision("r1".into()),
        risk: Risk::Irreversible,
        tool_revision: Revision(TOOL_REVISION.into()),
        attempt: ctx.attempt.clone(),
        call: call.clone(),
        approval: None,
        private_recall: false,
        owner: format!(
            "{}.{}.{}",
            ctx.attempt.run.0, ctx.attempt.generation.0, call.0
        ),
    }
}

pub fn tool_call(ctx: &RunContext, call: &CallId) -> ToolCall {
    ToolCall {
        attempt: ctx.attempt.clone(),
        call: call.clone(),
        tool: ToolId(TOOL.into()),
        args: JsonPayload(ARGS.into()),
    }
}

pub fn operator() -> bot_core::admin::Viewer {
    bot_core::admin::Viewer {
        principal: PrincipalId(OWNER.into()),
        operator: true,
    }
}

pub fn key() -> bot_core::grant::TriggerKey {
    bot_core::grant::TriggerKey {
        profile: ProfileId("personal".into()),
        agent: AgentId("main".into()),
        trigger: "notify".into(),
    }
}

/// 정지(`on`)·재개 명령을 보낸다(관리 API가 부르는 포트와 같다).
pub async fn pause(r: &Rig, grant: &str, epoch: i64, on: bool) -> Result<PauseOutcome, Error> {
    let cas = GrantCas {
        grant_id: grant.into(),
        epoch,
    };
    AdminAct::set_pause(&r.db.store, PrincipalId(OWNER.into()), key(), cas, on).await
}
