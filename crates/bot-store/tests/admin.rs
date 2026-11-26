mod support;

use bot_core::{command::*, id::*, port::*};
use support::*;

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
        .resume(profile(), run.clone(), operator(), "원인 확인함".into())
        .await
        .unwrap();
    assert_eq!(state_of(&db, &run).await, "ready");
    assert_eq!(db.scalar::<i32>("select attempts from bot_runs").await, 0);
    let again = db.store.claim().await.unwrap().unwrap();
    assert_eq!(
        (&again.attempt.run, again.attempt.generation),
        (&run, Generation(2)),
        "같은 run, 새 세대"
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    let audited = db
        .scalar::<String>(
            "select actor || ':' || verdict from bot_audit where action = 'run.resume'",
        )
        .await;
    assert_eq!(audited, "local:owner:원인 확인함");

    // 살아 있는 run이나 없는 run, 다른 프로필의 run은 재개할 수 없다
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
        .cancel(profile(), queued.clone(), operator(), "그만".into())
        .await
        .unwrap();
    assert_eq!(state_of(&db, &queued).await, "cancelled");
    assert!(
        db.store.claim().await.unwrap().is_none(),
        "취소된 run은 집히지 않는다"
    );

    db.store
        .cancel(profile(), running.clone(), operator(), "그만".into())
        .await
        .unwrap();
    assert_eq!(
        state_of(&db, &running).await,
        "leased",
        "실행 중이면 표시만 한다"
    );
    assert!(
        db.store
            .renew(got.attempt.clone())
            .await
            .unwrap()
            .cancel_requested
    );
    // 취소를 확인한 소유자는 재시도 없이 취소로 끝낸다
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
