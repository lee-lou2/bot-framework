//! `bot/pass` recovery and race tests: pre-commit crash handover, concurrent duplicates, non-candidate markers, and moved cursors.

use crate::support::{pass::*, *};
use bot_broker::ports::Ran;
use bot_core::{
    command::*,
    id::*,
    ledger::{FinishCall, Outcome},
    pass::direct_call_id,
    port::{Broker, Error},
    slack::{SlackSource, SourceKind},
    state::ConversationKind,
};

#[tokio::test]
async fn a_crash_before_the_cursor_commit_resumes_at_the_same_candidate_and_the_late_pass_is_fenced()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    let run = accept_chain(
        &db,
        "r1",
        chain(&["agenda.list", "agenda.find"], Some(default_target())),
    )
    .await;
    let dead = runs(&db).claim().await.unwrap().unwrap();
    // Drop the in-flight tool future mid-run to simulate a crash before cursor commit.
    let gate = rig.runner.hold_next();
    let mut inflight = Box::pin(call_at(&rig, &dead));
    let started = async {
        while rig.runner.jobs().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        done = &mut inflight => panic!("held runner finished unexpectedly: {done:?}"),
        () = started => {}
    }
    drop(inflight);
    drop(gate);

    let alive = db.takeover(&run).await;
    assert_eq!(
        (alive.work.pass_pos, &alive.work.target),
        (Some(0), &tool_target("agenda.list")),
        "cursor must stay at candidate 0"
    );
    assert!(alive.attempt.generation.0 > dead.attempt.generation.0);

    let late = calls(&db)
        .finish_pass(
            FinishCall {
                attempt: dead.attempt.clone(),
                call: direct_call_id(&run, 0),
                outcome: Outcome::Done(JsonPayload(PLAIN.into())),
            },
            0,
            Default::default(),
        )
        .await;
    assert!(matches!(late, Err(Error::Stale)), "{late:?}");
    assert_eq!(run_state(&db, &run).await.1, Some(0));
    let pass_audits =
        "select count(*) from bot_audit where action in ('call.pass', 'trigger.next')";
    assert_eq!(db.scalar::<i64>(pass_audits).await, 0);

    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    assert!(matches!(
        call_at(&rig, &alive).await,
        Ok(CallResult::Passed)
    ));
    assert_eq!(
        run_state(&db, &run).await,
        ("ready".into(), Some(1), "agenda.find".into())
    );
    let actions = audit_actions(&db).await;
    let count = |name: &str| actions.iter().filter(|a| *a == name).count();
    assert_eq!((count("call.pass"), count("trigger.next")), (1, 1));
    assert_eq!(
        rig.runner.jobs().len(),
        2,
        "dead attempt execution plus handover rerun"
    );
}

#[tokio::test]
async fn concurrent_duplicate_deliveries_pass_once_and_run_the_default_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let make = || chain(&["agenda.list"], Some(default_target()));
    let (a, b) = tokio::join!(
        accept_chain(&db, "r1", make()),
        accept_chain(&db, "r1", make())
    );
    assert_eq!(a, b, "concurrent duplicate intakes converge to one run");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);

    let ctx = runs(&db).claim().await.unwrap().unwrap();
    let (x, y) = tokio::join!(call_at(&rig, &ctx), call_at(&rig, &ctx));
    let passed = [&x, &y]
        .iter()
        .filter(|r| matches!(r, Ok(CallResult::Passed)))
        .count();
    assert_eq!(passed, 1, "{x:?} / {y:?}");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'call.pass'")
            .await,
        1
    );
    assert_eq!(
        run_state(&db, &run_of(&db).await).await,
        ("ready".into(), Some(1), "(agent)".into())
    );

    let default = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(default.work.target, default_target());
    assert!(
        runs(&db).claim().await.unwrap().is_none(),
        "default target cannot be claimed twice"
    );
}

async fn run_of(db: &TestDb) -> RunId {
    RunId(
        sqlx::query_scalar("select id from bot_runs limit 1")
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn a_marker_on_a_call_that_is_not_the_current_candidate_is_only_stripped() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let run = accept_chain(
        &db,
        "r1",
        chain(&["agenda.list", "agenda.find"], Some(default_target())),
    )
    .await;
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    let other = rig
        .broker
        .call(ToolCall {
            attempt: ctx.attempt.clone(),
            call: CallId(format!("{}:not-the-candidate", run.0)),
            tool: ToolId("agenda.list".into()),
            args: JsonPayload("{}".into()),
        })
        .await;
    let Ok(CallResult::Done(JsonPayload(body))) = other else {
        panic!("expected normal Done result: {other:?}");
    };
    assert!(
        !body.contains("bot/pass"),
        "marker must be stripped from result: {body}"
    );
    assert_eq!(
        run_state(&db, &run).await,
        ("leased".into(), Some(0), "agenda.list".into()),
        "cursor and target stay unchanged"
    );
    let pass_audits =
        "select count(*) from bot_audit where action in ('call.pass', 'trigger.next')";
    assert_eq!(db.scalar::<i64>(pass_audits).await, 0);
}

#[tokio::test]
async fn a_pass_for_a_cursor_that_already_moved_is_refused() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let run = accept_chain(
        &db,
        "r1",
        chain(&["agenda.list", "agenda.find"], Some(default_target())),
    )
    .await;
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    let gate = rig.runner.hold_next();
    let moved = async {
        while rig.runner.jobs().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        db.sql(&format!(
            "update bot_runs set pass_pos = 1 where id = '{}'",
            run.0
        ))
        .await;
        gate.add_permits(1);
    };
    let (result, ()) = tokio::join!(call_at(&rig, &ctx), moved);
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    assert_eq!(
        run_state(&db, &run).await.1,
        Some(1),
        "cursor must not advance further"
    );
    let pass_audits =
        "select count(*) from bot_audit where action in ('call.pass', 'trigger.next')";
    assert_eq!(db.scalar::<i64>(pass_audits).await, 0);
}

#[tokio::test]
async fn a_reaction_chain_that_passes_everywhere_calls_the_default_agent_once_even_when_redelivered()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let input = r#"{"event":"reaction_added","item_ts":"100.1","reaction":"white_check_mark"}"#;
    let default = Target::Agent {
        agent: AgentId("main".into()),
        instruction: input.into(),
    };
    let built = chain(&["agenda.list", "agenda.find"], Some(default.clone()));
    let source = SlackSource {
        kind: SourceKind::Reaction,
        team: "T1".into(),
        channel: "C1".into(),
        ts: "100.1".into(),
        thread_ts: None,
        event_id: "Ev-R1".into(),
        reaction: Some("white_check_mark".into()),
    };
    let deliver = |binding: &str| {
        let mut command = accept_as(
            "personal",
            "Ev-R1",
            "local:owner",
            ConversationKind::Channel,
            "C1",
            input,
        );
        command.target = built.candidates[0].target.clone();
        command.event.binding = BindingId(binding.into());
        let extras = AcceptExtras {
            source: Some(source.clone()),
            chain: Some(built.clone()),
        };
        (command, extras)
    };
    let (command, extras) = deliver("desk");
    let run = runs(&db).accept_with(command, extras).await.unwrap();

    for _ in 0..2 {
        let ctx = runs(&db).claim().await.unwrap().unwrap();
        assert!(matches!(call_at(&rig, &ctx).await, Ok(CallResult::Passed)));
    }
    let last = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(last.work.target, default);
    runs(&db).finish(last.attempt, done("ok")).await.unwrap();
    assert_eq!(run_state(&db, &run).await.0, "succeeded");

    // Redelivery under a renamed binding converges to the same run without re-running candidates.
    let (command, extras) = deliver("desk2");
    assert_eq!(runs(&db).accept_with(command, extras).await.unwrap(), run);
    assert!(runs(&db).claim().await.unwrap().is_none());
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(rig.runner.jobs().len(), 2, "each candidate ran once");
    let actions = audit_actions(&db).await;
    let count = |name: &str| actions.iter().filter(|a| *a == name).count();
    assert_eq!((count("run.accept"), count("trigger.select")), (1, 1));
}
