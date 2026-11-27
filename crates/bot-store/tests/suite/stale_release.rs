//! Stale-process handover (`release_stale` / `fail_or_release`) over PG: requeues without spending an attempt only when the handle's set is outdated and the attempt owns the lease.

use crate::support::*;
use bot_core::{
    command::{Failure, Retry},
    id::*,
    port::{Error, RunStore},
    profile::ActiveRevisions,
};
use bot_store::{Config, Store};
use sqlx::Connection;
use std::{collections::BTreeMap, time::Duration};

fn rev(value: &str) -> Revision {
    Revision(value.into())
}

async fn remap(db: &TestDb, from: &str, to: &str) {
    let profiles = BTreeMap::from([
        (profile_id(), rev("r1")),
        (ProfileId("work".into()), rev("r1")),
    ]);
    db.store
        .replace(Some(rev(from)), rev(to), profiles, vec![], BTreeMap::new())
        .await
        .unwrap();
}

fn one_attempt() -> Config {
    Config {
        max_attempts: 1,
        ..Config::default()
    }
}

async fn attempts(db: &TestDb, run: &RunId) -> i32 {
    db.scalar::<i32>(&format!(
        "select attempts from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

async fn claimed_by(db: &TestDb, handle: &Store, request: &str) -> bot_core::state::RunContext {
    runs(db)
        .accept(accept("personal", request, "once more"))
        .await
        .unwrap();
    RunStore::claim(handle).await.unwrap().expect("claimed run")
}

#[tokio::test]
async fn a_stale_worker_hands_the_last_attempt_back_without_spending_it_and_a_new_worker_finishes_it()
 {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    remap(&db, "set-1", "set-2").await;

    // Control: an ordinary failure on max_attempts=1 dead-letters immediately.
    let general = db.peer("general").await;
    let control = claimed_by(&db, &general, "control").await;
    assert!(matches!(
        RunStore::fail(&general, control.attempt.clone(), Failure::Transient("x")).await,
        Ok(Retry::Dead)
    ));

    old.release_stale(&run.attempt).await.unwrap();
    let id = &run.attempt.run;
    assert_eq!(state_of_run(&db, id).await, "ready");
    assert_eq!(
        attempts(&db, id).await,
        0,
        "stale handover across set replacement must not spend an attempt"
    );
    assert_eq!(
        db.scalar::<i64>(&format!(
            "select count(*) from bot_runs where id = '{}' and lease_owner is null and lease_until_ms is null",
            id.0
        ))
        .await,
        1,
        "lease must be cleared"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.handover'")
            .await,
        1
    );
    assert!(matches!(
        old.renew(run.attempt.clone()).await,
        Err(Error::Stale)
    ));

    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    let again = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("new worker claims");
    assert_eq!(&again.attempt.run, id);
    assert_eq!(again.attempt.generation.0, run.attempt.generation.0 + 1);
    assert_eq!(attempts(&db, id).await, 0);
    RunStore::finish(&new, again.attempt.clone(), done("finished on handover"))
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, id).await, "succeeded");
}

#[tokio::test]
async fn a_worker_of_the_current_set_is_refused_and_takes_the_normal_failure_path() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let current = db.peer("current").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &current, "r1").await;
    let id = &run.attempt.run;
    assert!(matches!(
        current.release_stale(&run.attempt).await,
        Err(Error::Denied)
    ));
    assert_eq!(state_of_run(&db, id).await, "leased");
    assert_eq!(attempts(&db, id).await, 0);
    assert!(matches!(
        db.store.release_stale(&run.attempt).await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        RunStore::fail(&current, run.attempt.clone(), Failure::Transient("x")).await,
        Ok(Retry::Dead)
    ));
    assert_eq!(state_of_run(&db, id).await, "dead");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.handover'")
            .await,
        0
    );
}

#[tokio::test]
async fn an_old_claim_or_generation_cannot_hand_a_run_back() {
    // Use default max_attempts so lease-expiry takeover stays alive rather than dead-lettering.
    let Some(db) = TestDb::with(Config::default()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    remap(&db, "set-1", "set-2").await;
    let owner = db.takeover(&id).await;
    assert!(owner.attempt.generation.0 > run.attempt.generation.0);
    assert!(matches!(
        old.release_stale(&run.attempt).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        state_of_run(&db, &id).await,
        "leased",
        "must not disturb new owner's lease"
    );
    let mut wrong_claim = owner.attempt.clone();
    wrong_claim.claim = Some(ClaimEpoch(owner.attempt.claim.unwrap().0 + 7));
    assert!(matches!(
        old.release_stale(&wrong_claim).await,
        Err(Error::Stale)
    ));
    let mut no_claim = owner.attempt.clone();
    no_claim.claim = None;
    assert!(matches!(
        old.release_stale(&no_claim).await,
        Err(Error::Stale)
    ));
    db.expire_lease(&id).await;
    assert!(matches!(
        old.release_stale(&owner.attempt).await,
        Err(Error::Stale)
    ));
    let fresh = db.peer("fresh-old").await.for_activation(rev("set-1"));
    let taken = RunStore::claim(&fresh).await;
    assert!(
        matches!(taken, Err(Error::Stale)),
        "stale handle must not claim: {taken:?}"
    );
    let next = db.peer("next").await.for_activation(rev("set-2"));
    let reclaimed = RunStore::claim(&next).await.unwrap().expect("reclaimed");
    let stale_again = db.peer("stale-again").await.for_activation(rev("set-1"));
    stale_again.release_stale(&reclaimed.attempt).await.unwrap();
    assert!(matches!(
        stale_again.release_stale(&reclaimed.attempt).await,
        Err(Error::Stale)
    ));
}

#[tokio::test]
async fn a_run_with_a_cancel_request_is_not_requeued_and_ends_as_cancelled_by_the_normal_path() {
    let Some(db) = TestDb::with(Config::default()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    remap(&db, "set-1", "set-2").await;
    db.sql(&format!(
        "update bot_runs set cancel_requested = true where id = '{}'",
        run.attempt.run.0
    ))
    .await;
    assert!(matches!(
        old.release_stale(&run.attempt).await,
        Err(Error::Denied)
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");
    assert!(matches!(
        RunStore::fail(&old, run.attempt.clone(), Failure::Transient("x")).await,
        Ok(Retry::Dead)
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "cancelled");
}

#[tokio::test]
async fn the_decision_waits_for_a_replacement_in_flight_and_sees_the_committed_set() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let mut connection = db.raw().await;
    let mut tx = connection.begin().await.unwrap();
    let writer: i32 = sqlx::query_scalar(
        "update bot_active set revision = 'set-2' where singleton returning pg_backend_pid()",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let attempt = run.attempt.clone();
    let task = tokio::spawn(async move { old.release_stale(&attempt).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "select exists(select 1 from pg_stat_activity where $1 = any(pg_blocking_pids(pid)))",
            )
            .bind(writer)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
            if blocked {
                break;
            }
            assert!(!task.is_finished(), "release_stale finished without waiting for replacement");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("release_stale must block behind in-flight replacement");
    tx.commit().await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
    assert_eq!(attempts(&db, &run.attempt.run).await, 0);
}

#[tokio::test]
async fn one_decision_hands_a_stale_owner_back_and_reports_everyone_else_as_an_ordinary_failure() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let first = claimed_by(&db, &old, "r1").await;
    remap(&db, "set-1", "set-2").await;
    let retry = old
        .fail_or_release(&first.attempt, Failure::Permanent("model_denied"))
        .await
        .unwrap();
    assert_eq!(retry, Retry::After(Duration::ZERO));
    assert_eq!(state_of_run(&db, &first.attempt.run).await, "ready");
    assert_eq!(attempts(&db, &first.attempt.run).await, 0);
    assert!(matches!(
        old.fail_or_release(&first.attempt, Failure::Transient("x"))
            .await,
        Err(Error::Stale)
    ));
    let current = db.peer("current").await.for_activation(rev("set-2"));
    let second = RunStore::claim(&current)
        .await
        .unwrap()
        .expect("new worker claims");
    assert_eq!(second.attempt.run, first.attempt.run);
    assert!(matches!(
        current
            .fail_or_release(&second.attempt, Failure::Transient("x"))
            .await,
        Ok(Retry::Dead)
    ));
    assert_eq!(state_of_run(&db, &second.attempt.run).await, "dead");
    let plain = claimed_by(&db, &db.store, "r2").await;
    assert!(matches!(
        db.store
            .fail_or_release(&plain.attempt, Failure::Permanent("x"))
            .await,
        Ok(Retry::Dead)
    ));
    let old2 = db.peer("old-2").await.for_activation(rev("set-2"));
    let third = claimed_by(&db, &old2, "r3").await;
    remap(&db, "set-2", "set-3").await;
    db.sql(&format!(
        "update bot_runs set cancel_requested = true where id = '{}'",
        third.attempt.run.0
    ))
    .await;
    assert!(matches!(
        old2.fail_or_release(&third.attempt, Failure::Transient("x"))
            .await,
        Ok(Retry::Dead)
    ));
    assert_eq!(state_of_run(&db, &third.attempt.run).await, "cancelled");
}

/// Builds a store handle with a short `lock_timeout` to simulate a blocked active-set check.
async fn impatient(db: &TestDb, millis: u32) -> Store {
    use sqlx::postgres::PgConnectOptions;
    use std::str::FromStr;
    let url = std::env::var("BOT_TEST_DATABASE_URL").expect("test DB URL");
    let options = PgConnectOptions::from_str(&url)
        .unwrap()
        .options([
            ("search_path", db.schema.as_str()),
            ("lock_timeout", &millis.to_string()),
        ])
        .application_name(&db.schema);
    let pool = Store::pool_options()
        .max_connections(4)
        .connect_with(options)
        .await
        .unwrap();
    Store::from_pool(pool, one_attempt())
}

#[tokio::test]
async fn a_lock_timeout_in_the_decision_records_no_failure_and_the_repeat_hands_the_run_back() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = impatient(&db, 300).await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    // Hold bot_active so `FOR SHARE` in fail_or_release hits lock_timeout.
    let mut connection = db.raw().await;
    let mut tx = connection.begin().await.unwrap();
    sqlx::query("update bot_active set revision = 'set-2' where singleton")
        .execute(&mut *tx)
        .await
        .unwrap();
    let timed_out = old
        .fail_or_release(&run.attempt, Failure::Permanent("model_denied"))
        .await;
    assert!(
        matches!(timed_out, Err(Error::Unavailable)),
        "{timed_out:?}"
    );
    assert_eq!(
        state_of_run(&db, &id).await,
        "leased",
        "must not record a failure when set check times out"
    );
    assert_eq!(attempts(&db, &id).await, 0);
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_audit where action in ('run.retry', 'run.dead', 'run.fail')"
        )
        .await,
        0
    );
    tx.commit().await.unwrap();
    let again = old
        .fail_or_release(&run.attempt, Failure::Permanent("model_denied"))
        .await;
    assert!(
        matches!(again, Ok(Retry::After(d)) if d.is_zero()),
        "{again:?}"
    );
    assert_eq!(state_of_run(&db, &id).await, "ready");
    assert_eq!(attempts(&db, &id).await, 0);
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    let resumed = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("handover claim");
    RunStore::finish(&new, resumed.attempt.clone(), done("finished"))
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, &id).await, "succeeded");
}
