use crate::support::*;
use bot_core::{command::*, id::*, ledger::*, port::*, state::*};

fn owner() -> PrincipalId {
    PrincipalId("local:owner".into())
}

async fn phase(db: &TestDb, at: &Attempt, id: &str) -> CallPhase {
    calls(db)
        .begin(at.clone(), call(id), tool("web.search"), [1; 32])
        .await
        .unwrap()
        .phase
}

#[tokio::test]
async fn begin_is_idempotent_and_refuses_a_different_tool_or_hash() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    let again = begin_call(&db, &run.attempt, "c1").await;
    assert!(matches!(again.phase, CallPhase::Open { approval: None }));
    let other_tool = calls(&db)
        .begin(run.attempt.clone(), call("c1"), tool("a.b"), [1; 32])
        .await;
    let other_hash = calls(&db)
        .begin(run.attempt.clone(), call("c1"), tool("web.search"), [2; 32])
        .await;
    assert!(
        matches!(other_tool, Err(Error::Conflict)) && matches!(other_hash, Err(Error::Conflict))
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_calls").await, 1);
}

#[tokio::test]
async fn every_write_is_fenced_to_the_current_generation_and_profile() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let old = leased(&db, "r").await;
    begin_call(&db, &old.attempt, "c1").await;
    let new = db.takeover(&old.attempt.run).await;
    assert_eq!(new.attempt.generation, Generation(2));

    let stale = |r: Result<_, Error>| matches!(r, Err(Error::Stale));
    assert!(stale(
        calls(&db).context(old.attempt.clone()).await.map(|_| ())
    ));
    assert!(stale(
        calls(&db)
            .begin(old.attempt.clone(), call("c2"), tool("web.search"), [1; 32])
            .await
            .map(|_| ())
    ));
    assert!(stale(
        calls(&db).start(start_call(&old.attempt, "c1", None)).await
    ));
    let finish = FinishCall {
        attempt: old.attempt.clone(),
        call: call("c1"),
        outcome: Outcome::Blocked,
    };
    assert!(stale(calls(&db).finish(finish).await));
    assert!(stale(
        calls(&db)
            .open_approval(open_approval(binding(&old.attempt, "c1", "n-old")))
            .await
    ));
    assert!(matches!(
        begin_call(&db, &new.attempt, "c1").await.phase,
        CallPhase::Open { .. }
    ));

    let foreign = Attempt {
        profile: ProfileId("work".into()),
        ..new.attempt.clone()
    };
    assert!(matches!(
        calls(&db).context(foreign).await,
        Err(Error::Denied)
    ));
    let ghost = Attempt {
        run: RunId("ghost".into()),
        ..new.attempt.clone()
    };
    assert!(matches!(
        calls(&db).context(ghost).await,
        Err(Error::Missing)
    ));
}

#[tokio::test]
async fn context_is_read_from_stored_state() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    let context = calls(&db).context(run.attempt.clone()).await.unwrap();
    assert_eq!(
        (context.principal.0.as_str(), context.agent.0.as_str()),
        ("local:owner", "main")
    );
    assert_eq!(context.attempt, run.attempt);
    assert!(!context.private_memory_read);
    db.sql("update bot_runs set private_memory_read = true")
        .await;
    assert!(
        calls(&db)
            .context(run.attempt.clone())
            .await
            .unwrap()
            .private_memory_read
    );
}

#[tokio::test]
async fn a_cancelled_run_cannot_start_anything() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    db.store
        .cancel(
            profile_id(),
            run.attempt.run.clone(),
            owner(),
            "stop".into(),
        )
        .await
        .unwrap();
    assert!(matches!(
        calls(&db).start(start_call(&run.attempt, "c1", None)).await,
        Err(Error::Stale)
    ));
}

#[tokio::test]
async fn a_private_flag_raised_between_judgement_and_start_is_a_conflict() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    // Simulate a concurrent run in the tree reading private memory after broker authorization.
    db.sql("update bot_runs set private_memory_read = true")
        .await;
    assert!(matches!(
        calls(&db).start(start_call(&run.attempt, "c1", None)).await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        phase(&db, &run.attempt, "c1").await,
        CallPhase::Open { .. }
    ));
    let mut aware = start_call(&run.attempt, "c1", None);
    aware.private_recall = true;
    calls(&db).start(aware).await.unwrap();
}

#[tokio::test]
async fn a_started_call_cannot_start_again_without_a_proven_release() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = leased(&db, "r").await;
    begin_call(&db, &first.attempt, "c1").await;
    calls(&db)
        .start(start_call(&first.attempt, "c1", None))
        .await
        .unwrap();
    assert!(matches!(
        calls(&db)
            .start(start_call(&first.attempt, "c1", None))
            .await,
        Err(Error::Conflict)
    ));

    let second = db.takeover(&first.attempt.run).await;
    assert!(matches!(
        phase(&db, &second.attempt, "c1").await,
        CallPhase::Started {
            generation: Generation(1),
            ..
        }
    ));
    assert!(matches!(
        calls(&db)
            .start(start_call(&second.attempt, "c1", None))
            .await,
        Err(Error::Conflict)
    ));
    // Once proven unexecuted via Released, the new generation may start the call again.
    let release = FinishCall {
        attempt: second.attempt.clone(),
        call: call("c1"),
        outcome: Outcome::Released,
    };
    calls(&db).finish(release).await.unwrap();
    calls(&db)
        .start(start_call(&second.attempt, "c1", None))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'call.start'")
            .await,
        2
    );
}

#[tokio::test]
async fn finish_follows_the_phase_matrix() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    let finish = |c: &str, outcome| FinishCall {
        attempt: run.attempt.clone(),
        call: call(c),
        outcome,
    };
    let payload = || JsonPayload(r#"{"ok":true}"#.into());
    let conflict = |r: Result<(), Error>| matches!(r, Err(Error::Conflict));

    begin_call(&db, &run.attempt, "open").await;
    assert!(conflict(
        calls(&db)
            .finish(finish("open", Outcome::Done(payload())))
            .await
    ));
    calls(&db)
        .finish(finish("open", Outcome::Blocked))
        .await
        .unwrap();
    assert!(matches!(
        phase(&db, &run.attempt, "open").await,
        CallPhase::Denied
    ));
    assert!(
        conflict(calls(&db).finish(finish("open", Outcome::Blocked)).await),
        "closed call cannot be finished again"
    );

    begin_call(&db, &run.attempt, "done").await;
    calls(&db)
        .start(start_call(&run.attempt, "done", None))
        .await
        .unwrap();
    calls(&db)
        .finish(finish("done", Outcome::Done(payload())))
        .await
        .unwrap();
    let CallPhase::Done(stored) = phase(&db, &run.attempt, "done").await else {
        panic!("expected Done phase")
    };
    assert_eq!(stored.0, r#"{"ok":true}"#);
    assert!(conflict(
        calls(&db).finish(finish("done", Outcome::Released)).await
    ));
}

#[tokio::test]
async fn a_held_call_parks_the_run_and_waits_for_an_operator_verdict() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    let held = FinishCall {
        attempt: run.attempt.clone(),
        call: call("c1"),
        outcome: Outcome::Held("runner_error".into()),
    };
    calls(&db).finish(held).await.unwrap();
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    assert_eq!(
        db.scalar::<String>("select wait_kind from bot_runs").await,
        "hold"
    );
    assert_eq!(db.scalar::<i32>("select attempts from bot_runs").await, 0);
    assert!(matches!(
        runs(&db).renew(run.attempt.clone()).await,
        Err(Error::Stale)
    ));
    // Original started_generation and owner persist through Held for recovery checks.
    assert_eq!(
        db.scalar::<i64>("select started_generation from bot_calls")
            .await,
        1
    );
    assert_eq!(
        db.scalar::<String>("select owner from bot_calls").await,
        format!("{}.1.c1", run.attempt.run.0)
    );

    assert!(
        runs(&db).claim().await.unwrap().is_none(),
        "waiting run must not be claimable"
    );
    db.sql("update bot_calls set resolution = 'executed', receipt = 'r-1'")
        .await;
    db.sql("update bot_runs set state = 'ready', wait_kind = null, keep_generation = true")
        .await;
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_resumed(&run.attempt, &resumed.attempt);
    let done = FinishCall {
        attempt: resumed.attempt.clone(),
        call: call("c1"),
        outcome: Outcome::Done(JsonPayload(r#"{"receipt":"r-1"}"#.into())),
    };
    calls(&db).finish(done).await.unwrap();
    assert!(matches!(
        phase(&db, &resumed.attempt, "c1").await,
        CallPhase::Done(_)
    ));
}

#[tokio::test]
async fn meeting_an_unresolved_hold_again_parks_the_new_attempt() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = leased(&db, "r").await;
    begin_call(&db, &first.attempt, "c1").await;
    calls(&db)
        .start(start_call(&first.attempt, "c1", None))
        .await
        .unwrap();
    let held = FinishCall {
        attempt: first.attempt.clone(),
        call: call("c1"),
        outcome: Outcome::Held("x".into()),
    };
    calls(&db).finish(held).await.unwrap();
    db.sql("update bot_runs set state = 'ready', wait_kind = null, keep_generation = true")
        .await;
    let again = runs(&db).claim().await.unwrap().unwrap();
    let record = begin_call(&db, &again.attempt, "c1").await;
    assert!(matches!(
        record.phase,
        CallPhase::Held {
            resolution: None,
            ..
        }
    ));
    assert_eq!(
        state_of_run(&db, &again.attempt.run).await,
        "waiting",
        "run re-parks when hold has no resolution"
    );
    assert_eq!(db.scalar::<i32>("select attempts from bot_runs").await, 0);
}

#[tokio::test]
async fn start_records_the_real_risk_and_tool_revision() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    let mut start = start_call(&run.attempt, "c1", None);
    start.risk = Risk::Irreversible;
    start.tool_revision = Revision("t9@image-1".into());
    calls(&db).start(start).await.unwrap();
    assert_eq!(
        db.scalar::<String>("select risk || ':' || tool_revision from bot_calls")
            .await,
        "irreversible:t9@image-1"
    );
}
