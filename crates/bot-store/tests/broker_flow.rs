//! 실제 BrokerService를 PG 원장 위에서 돌려 승인 흐름의 계약 수락을 확인한다:
//! 승인 요청(run 대기·리스 해제) → 대기 중 승인 클릭(approval_context) → 같은 세대 재개(새 claim 토큰) → 승인 소비·한 번 실행.

mod support;

use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, RecordingTracer, snapshot, tool},
};
use bot_core::{
    command::*,
    id::*,
    ledger::ApprovalState,
    port::{AuditLog, Broker, DelegationStore, Error, ScheduleStore},
    profile::TriggerTarget,
    state::Risk,
};
use std::sync::Arc;
use support::*;

const OWNER: &str = "local:owner";
const SERVICE: &str = "service:morning";

struct Rig {
    broker: BrokerService,
    runner: Arc<FakeRunner>,
}

fn rig(db: &TestDb) -> Rig {
    let catalog = Arc::new(FakeCatalog::new());
    let mut world = snapshot("personal", "r1")
        .role("owner", &["run", "calendar.delete"])
        .member(OWNER, &["owner"])
        .member(SERVICE, &["owner"])
        .policy_scopes(&["run", "calendar.delete"])
        .cron_trigger("main", "morning", SERVICE, Some(OWNER))
        .tool(
            "main",
            "agenda.delete",
            tool(Risk::Irreversible, &["calendar.delete"]),
        )
        .tool(
            "main/research",
            "agenda.delete",
            tool(Risk::Irreversible, &["calendar.delete"]),
        )
        .build();
    Arc::make_mut(&mut world)
        .agents
        .get_mut(&AgentId("main".into()))
        .unwrap()
        .triggers[0]
        .target = TriggerTarget::Instruction("review".into());
    catalog.publish(world);
    let store = Arc::new(db.store.clone());
    let runner = Arc::new(FakeRunner::new());
    let schedules = store.clone();
    let broker = BrokerService::new(
        Parts {
            catalog,
            ledger: store.clone(),
            effects: store.clone(),
            memory: store.clone(),
            audit: store,
            runner: runner.clone(),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    )
    .with_schedules(schedules);
    Rig { broker, runner }
}

fn delete_call(attempt: &Attempt) -> ToolCall {
    ToolCall {
        attempt: attempt.clone(),
        call: call("c1"),
        tool: tool_id("agenda.delete"),
        args: JsonPayload(r#"{"event":"walk"}"#.into()),
    }
}

fn tool_id(id: &str) -> ToolId {
    ToolId(id.into())
}

#[tokio::test]
async fn a_delegated_cron_tool_keeps_the_service_requester_after_human_approval() {
    use std::time::{Duration, UNIX_EPOCH};

    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    let target = Target::Agent {
        agent: AgentId("main".into()),
        instruction: "review".into(),
    };
    let grants = ["run", "calendar.delete"];
    let mut spec = trigger_spec("morning", "* * * * *", false);
    spec.target = target.clone();
    spec.scopes = scopes(&grants);
    spec.approver = Some(PrincipalId(OWNER.into()));
    db.publish_schedule(schedule_set("personal", "r1", vec![spec]))
        .await
        .unwrap();
    db.backdate_schedule().await;
    let ms = db.now_ms().await - 60_000;
    let mut command = occurrence_accept("personal", "morning", ms, target, &grants);
    command.approver = Some(PrincipalId(OWNER.into()));
    db.store
        .accept_occurrence(
            Occurrence {
                profile: profile_id(),
                agent: AgentId("main".into()),
                trigger: "morning".into(),
                scheduled_at: UNIX_EPOCH + Duration::from_millis(ms as u64),
            },
            command,
        )
        .await
        .unwrap();
    let parent = runs(&db).claim().await.unwrap().unwrap();
    let child = db
        .store
        .delegate(Delegate {
            checked_revision: bot_core::id::Revision("r1".into()),
            parent: parent.attempt,
            step: "review".into(),
            child: AgentId("main/research".into()),
            instruction: "review".into(),
            scopes: scopes(&["calendar.delete"]),
        })
        .await
        .unwrap();
    let next = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(next.attempt.run, child);
    let CallResult::Awaiting(nonce) = rig.broker.call(delete_call(&next.attempt)).await.unwrap()
    else {
        panic!("human approval required")
    };
    assert!(rig.runner.jobs().is_empty());
    rig.broker
        .decide(nonce, PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(resumed.attempt.run, child);
    assert!(matches!(
        rig.broker
            .call(delete_call(&resumed.attempt))
            .await
            .unwrap(),
        CallResult::Done(_)
    ));
    let jobs = rig.runner.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].principal.0, SERVICE);
}

#[tokio::test]
async fn an_approved_irreversible_call_runs_exactly_once_after_the_parked_run_resumes() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish().await;
    let rig = rig(&db);
    let mut accept = accept("personal", "req", "산책 일정을 삭제해줘");
    accept.scopes = scopes(&["run", "calendar.delete"]);
    accept.approver = Some(PrincipalId(OWNER.into()));
    runs(&db).accept(accept).await.unwrap();
    let first = runs(&db).claim().await.unwrap().unwrap();

    // 승인이 필요한 호출은 실행하지 않고 대기한다: run은 대기로 넘어가 워커 리스를 놓는다
    let CallResult::Awaiting(nonce) = rig.broker.call(delete_call(&first.attempt)).await.unwrap()
    else {
        panic!("승인 대기여야 한다")
    };
    assert!(rig.runner.jobs().is_empty(), "승인 전 실행 0건");
    assert_eq!(state_of_run(&db, &first.attempt.run).await, "waiting");
    assert!(matches!(
        runs(&db).renew(first.attempt.clone()).await,
        Err(Error::Stale)
    ));

    // 대기 중에도 승인 클릭이 통한다(승인 버튼은 approval_context로 읽는다). 낯선 사람의 클릭은 거절된다
    let stranger = rig
        .broker
        .decide(nonce.clone(), PrincipalId("local:guest".into()), true)
        .await;
    assert!(matches!(stranger, Err(Error::Denied)));
    rig.broker
        .decide(nonce.clone(), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    rig.broker
        .decide(nonce.clone(), PrincipalId(OWNER.into()), true)
        .await
        .unwrap(); // 같은 클릭의 반복은 같은 결과
    assert_eq!(
        calls(&db).approval(nonce.clone()).await.unwrap().state,
        ApprovalState::Approved
    );
    assert_eq!(state_of_run(&db, &first.attempt.run).await, "ready");
    assert!(rig.runner.jobs().is_empty(), "결정만으로는 실행하지 않는다");

    // 같은 세대로 재개하고(claim 토큰만 오른다), 같은 호출 ID로 다시 들어오면 승인을 소비하고 한 번 실행한다
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_resumed(&first.attempt, &resumed.attempt);
    assert!(
        matches!(
            rig.broker.call(delete_call(&first.attempt)).await,
            Err(Error::Stale)
        ),
        "옛 attempt의 호출은 거절"
    );
    assert!(matches!(
        runs(&db).renew(first.attempt.clone()).await,
        Err(Error::Stale)
    ));
    let CallResult::Done(result) = rig
        .broker
        .call(delete_call(&resumed.attempt))
        .await
        .unwrap()
    else {
        panic!("실행되어야 한다")
    };
    assert_eq!(rig.runner.jobs().len(), 1);
    assert_eq!(rig.runner.jobs()[0].principal.0, OWNER);
    assert_eq!(
        calls(&db).approval(nonce.clone()).await.unwrap().state,
        ApprovalState::Consumed
    );

    // 재시도(같은 호출 ID)는 저장된 결과를 돌려주고 다시 실행하지 않는다
    let CallResult::Done(again) = rig
        .broker
        .call(delete_call(&resumed.attempt))
        .await
        .unwrap()
    else {
        panic!("저장된 결과여야 한다")
    };
    assert_eq!((again, rig.runner.jobs().len()), (result, 1));
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}
