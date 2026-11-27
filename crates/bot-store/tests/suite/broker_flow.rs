//! End-to-end BrokerService approval flow on PostgreSQL: park, decide while parked, resume on same generation, consume once.

use crate::support::*;
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
    let mut accept = accept("personal", "req", "Delete the walk event");
    accept.scopes = scopes(&["run", "calendar.delete"]);
    accept.approver = Some(PrincipalId(OWNER.into()));
    runs(&db).accept(accept).await.unwrap();
    let first = runs(&db).claim().await.unwrap().unwrap();

    let CallResult::Awaiting(nonce) = rig.broker.call(delete_call(&first.attempt)).await.unwrap()
    else {
        panic!("expected awaiting approval")
    };
    assert!(
        rig.runner.jobs().is_empty(),
        "must not execute before approval"
    );
    assert_eq!(state_of_run(&db, &first.attempt.run).await, "waiting");
    assert!(matches!(
        runs(&db).renew(first.attempt.clone()).await,
        Err(Error::Stale)
    ));

    // Decisions succeed while parked via approval_context; non-approvers are denied.
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
        .unwrap();
    assert_eq!(
        calls(&db).approval(nonce.clone()).await.unwrap().state,
        ApprovalState::Approved
    );
    assert_eq!(state_of_run(&db, &first.attempt.run).await, "ready");
    assert!(
        rig.runner.jobs().is_empty(),
        "decision alone must not execute"
    );

    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_resumed(&first.attempt, &resumed.attempt);
    assert!(
        matches!(
            rig.broker.call(delete_call(&first.attempt)).await,
            Err(Error::Stale)
        ),
        "stale attempt must be rejected"
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
        panic!("expected call to run")
    };
    assert_eq!(rig.runner.jobs().len(), 1);
    assert_eq!(rig.runner.jobs()[0].principal.0, OWNER);
    assert_eq!(
        calls(&db).approval(nonce.clone()).await.unwrap().state,
        ApprovalState::Consumed
    );

    let CallResult::Done(again) = rig
        .broker
        .call(delete_call(&resumed.attempt))
        .await
        .unwrap()
    else {
        panic!("expected cached result")
    };
    assert_eq!((again, rig.runner.jobs().len()), (result, 1));
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}
