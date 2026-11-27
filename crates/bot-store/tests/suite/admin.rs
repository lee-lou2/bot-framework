use crate::support::*;
use bot_core::{command::*, id::*, port::*};

fn profile() -> ProfileId {
    ProfileId("personal".into())
}

fn operator() -> PrincipalId {
    PrincipalId("local:owner".into())
}

async fn state_of(db: &TestDb, run: &RunId) -> String {
    db.scalar::<String>(&format!(
        "select state from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

#[tokio::test]
async fn dead_letter_resume_keeps_the_logical_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = db
        .store
        .accept(accept("personal", "r1", "hi"))
        .await
        .unwrap();
    let got = db.store.claim().await.unwrap().unwrap();
    db.store
        .fail(got.attempt, Failure::Permanent("invalid_output"))
        .await
        .unwrap();
    assert_eq!(state_of(&db, &run).await, "dead");

    db.store
        .resume(
            profile(),
            run.clone(),
            operator(),
            "root cause verified".into(),
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, &run).await, "ready");
    assert_eq!(db.scalar::<i32>("select attempts from bot_runs").await, 0);
    let again = db.store.claim().await.unwrap().unwrap();
    assert_eq!(
        (&again.attempt.run, again.attempt.generation),
        (&run, Generation(2)),
        "same run, new generation"
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    let audited = db
        .scalar::<String>(
            "select actor || ':' || verdict from bot_audit where action = 'run.resume'",
        )
        .await;
    assert_eq!(audited, "local:owner:root cause verified");

    let live = db
        .store
        .resume(profile(), run.clone(), operator(), "x".into())
        .await;
    assert!(matches!(live, Err(Error::Conflict)));
    let missing = db
        .store
        .resume(profile(), RunId("nope".into()), operator(), "x".into())
        .await;
    assert!(matches!(missing, Err(Error::Missing)));
    let foreign = db
        .store
        .resume(ProfileId("work".into()), run, operator(), "x".into())
        .await;
    assert!(matches!(foreign, Err(Error::Missing)));
}

#[tokio::test]
async fn cancel_ends_queued_runs_and_flags_running_ones() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let running = db
        .store
        .accept(accept("personal", "r", "hi"))
        .await
        .unwrap();
    let got = db.store.claim().await.unwrap().unwrap();
    assert_eq!(got.attempt.run, running);
    let queued = db
        .store
        .accept(accept("personal", "q", "hi"))
        .await
        .unwrap();

    db.store
        .cancel(profile(), queued.clone(), operator(), "stop".into())
        .await
        .unwrap();
    assert_eq!(state_of(&db, &queued).await, "cancelled");
    assert!(
        db.store.claim().await.unwrap().is_none(),
        "cancelled run must not be claimed"
    );

    db.store
        .cancel(profile(), running.clone(), operator(), "stop".into())
        .await
        .unwrap();
    assert_eq!(
        state_of(&db, &running).await,
        "leased",
        "running run is only flagged for cancellation"
    );
    assert!(
        db.store
            .renew(got.attempt.clone())
            .await
            .unwrap()
            .cancel_requested
    );
    // Lease owner observing cancel finishes as cancelled without retry.
    let outcome = db
        .store
        .fail(got.attempt, Failure::Permanent("cancelled"))
        .await
        .unwrap();
    assert_eq!(outcome, Retry::Dead);
    assert_eq!(state_of(&db, &running).await, "cancelled");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.cancel'")
            .await,
        2
    );

    let missing = db
        .store
        .cancel(profile(), RunId("nope".into()), operator(), "x".into())
        .await;
    assert!(matches!(missing, Err(Error::Missing)));
}
