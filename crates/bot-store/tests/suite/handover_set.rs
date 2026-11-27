//! Lease-expiry handover and host active set (PG): reclaims across an active-set replacement forgive one attempt.

use crate::support::*;
use bot_core::{
    id::*,
    port::{Error, RunStore},
    profile::ActiveRevisions,
    state::RunContext,
};
use bot_store::{Config, Store};
use sqlx::{Connection, postgres::PgPoolOptions};
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

/// Attempt limit of 1 so any un-forgiven reclaim dead-letters immediately.
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

async fn recorded_set(db: &TestDb, run: &RunId) -> Option<String> {
    db.scalar::<Option<String>>(&format!(
        "select claim_set from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

async fn reclaims(db: &TestDb, word: &str) -> i64 {
    db.scalar::<i64>(&format!(
        "select count(*) from bot_audit where action = 'run.reclaim' and verdict = '{word}'"
    ))
    .await
}

async fn claimed_by(db: &TestDb, handle: &Store, request: &str) -> RunContext {
    runs(db)
        .accept(accept("personal", request, "once more"))
        .await
        .unwrap();
    RunStore::claim(handle).await.unwrap().expect("claimed run")
}

/// Holds an uncommitted update on `bot_active` to simulate an in-flight set replacement.
async fn replacement_in_flight<'a>(
    connection: &'a mut sqlx::PgConnection,
    to: &str,
) -> (sqlx::Transaction<'a, sqlx::Postgres>, i32) {
    let mut tx = connection.begin().await.unwrap();
    let writer: i32 = sqlx::query_scalar(
        "update bot_active set revision = $1 where singleton returning pg_backend_pid()",
    )
    .bind(to)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    (tx, writer)
}

#[tokio::test]
async fn a_lease_that_ended_while_the_active_set_was_replaced_does_not_spend_the_attempt() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    assert_eq!(
        recorded_set(&db, &id).await.as_deref(),
        Some("set-1"),
        "claim records the handle's pinned set in the same UPDATE"
    );

    // Block stale release behind an in-flight replacement until it times out, then let the lease expire.
    let mut connection = db.raw().await;
    let (tx, _writer) = replacement_in_flight(&mut connection, "set-2").await;
    let attempt = run.attempt.clone();
    let handed_back =
        tokio::time::timeout(Duration::from_millis(500), old.release_stale(&attempt)).await;
    assert!(
        handed_back.is_err(),
        "release must block behind in-flight replacement and time out"
    );
    assert_eq!(state_of_run(&db, &id).await, "leased");
    tx.commit().await.unwrap();
    db.expire_lease(&id).await;

    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    let again = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("new worker reclaims");
    assert_eq!(again.attempt.run, id);
    assert_eq!(
        attempts(&db, &id).await,
        0,
        "expiry across set replacement must not spend an attempt"
    );
    assert_eq!(
        recorded_set(&db, &id).await.as_deref(),
        Some("set-2"),
        "claim_set updates to the new claim's set"
    );
    assert_eq!(
        again.attempt.generation.0,
        run.attempt.generation.0 + 1,
        "reclaim advances generation"
    );
    assert!(again.attempt.claim.unwrap().0 > run.attempt.claim.unwrap().0);
    assert_eq!(reclaims(&db, "set_replaced").await, 1);
    assert_eq!(reclaims(&db, "lease_expired").await, 0);
    assert!(
        matches!(old.renew(run.attempt.clone()).await, Err(Error::Stale)),
        "old claim is fenced"
    );
    RunStore::finish(&new, again.attempt.clone(), done("finished on handover"))
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, &id).await, "succeeded");
}

#[tokio::test]
async fn an_expiry_under_the_same_set_spends_the_attempt_and_dead_letters_at_the_limit() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    db.expire_lease(&id).await;
    let same = db.peer("same-set").await.for_activation(rev("set-1"));
    assert!(
        RunStore::claim(&same).await.unwrap().is_none(),
        "same-set expiry spends the attempt and dead-letters at limit"
    );
    assert_eq!(state_of_run(&db, &id).await, "dead");
    assert_eq!(
        db.scalar::<String>("select error from bot_runs").await,
        "lease_expired"
    );
}

#[tokio::test]
async fn a_row_without_a_recorded_set_spends_the_attempt_even_when_the_set_changed() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    // Simulate a legacy row created before claim_set existed.
    db.sql(&format!(
        "update bot_runs set claim_set = null where id = '{}'",
        id.0
    ))
    .await;
    remap(&db, "set-1", "set-2").await;
    db.expire_lease(&id).await;
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    assert!(RunStore::claim(&new).await.unwrap().is_none());
    assert_eq!(state_of_run(&db, &id).await, "dead");
}

#[tokio::test]
async fn an_unpinned_handle_neither_records_a_set_nor_forgives_one() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let plain = claimed_by(&db, &db.store, "plain").await;
    let plain_id = plain.attempt.run.clone();
    assert_eq!(recorded_set(&db, &plain_id).await, None);
    remap(&db, "set-1", "set-2").await;
    db.expire_lease(&plain_id).await;
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    assert!(RunStore::claim(&new).await.unwrap().is_none());
    assert_eq!(state_of_run(&db, &plain_id).await, "dead");

    // Even if the prior claim was pinned, an unpinned reclaiming handle cannot verify the current set.
    let pinned = db.peer("pinned").await.for_activation(rev("set-2"));
    let run = claimed_by(&db, &pinned, "pinned").await;
    let id = run.attempt.run.clone();
    assert_eq!(recorded_set(&db, &id).await.as_deref(), Some("set-2"));
    remap(&db, "set-2", "set-3").await;
    db.expire_lease(&id).await;
    assert!(RunStore::claim(&db.store).await.unwrap().is_none());
    assert_eq!(state_of_run(&db, &id).await, "dead");
}

#[tokio::test]
async fn only_the_first_expiry_after_a_replacement_is_forgiven() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    remap(&db, "set-1", "set-2").await;
    db.expire_lease(&id).await;
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    let first = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("first handover forgiven");
    assert_eq!(attempts(&db, &first.attempt.run).await, 0);
    db.expire_lease(&id).await;
    let next = db.peer("next-worker").await.for_activation(rev("set-2"));
    assert!(
        RunStore::claim(&next).await.unwrap().is_none(),
        "second expiry under the same set spends the attempt"
    );
    assert_eq!(state_of_run(&db, &id).await, "dead");
    assert_eq!(
        db.scalar::<String>("select error from bot_runs").await,
        "lease_expired"
    );
}

#[tokio::test]
async fn a_cancel_request_still_ends_the_run_when_the_set_changed() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    remap(&db, "set-1", "set-2").await;
    db.sql(&format!(
        "update bot_runs set cancel_requested = true where id = '{}'",
        id.0
    ))
    .await;
    db.expire_lease(&id).await;
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    assert!(
        RunStore::claim(&new).await.unwrap().is_none(),
        "cancellation takes precedence over handover"
    );
    assert_eq!(state_of_run(&db, &id).await, "cancelled");
}

#[tokio::test]
async fn an_unreadable_set_writes_nothing_and_the_retry_after_the_replacement_is_forgiven() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    db.expire_lease(&id).await;
    let before = (
        state_of_run(&db, &id).await,
        attempts(&db, &id).await,
        db.scalar::<i64>("select claim_epoch from bot_runs").await,
        db.scalar::<i64>("select count(*) from bot_audit").await,
    );

    // Hold bot_active so the set check times out under lock_timeout.
    let mut connection = db.raw().await;
    let (tx, _writer) = replacement_in_flight(&mut connection, "set-2").await;
    let options = (*db.store.pool().connect_options())
        .clone()
        .options([("lock_timeout", "300")]);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .unwrap();
    let hasty = Store::from_pool(pool, db.store.config().clone()).for_activation(rev("set-1"));
    let unread = RunStore::claim(&hasty).await;
    assert!(
        matches!(unread, Err(Error::Unavailable)),
        "unreadable set must fail without claiming: {unread:?}"
    );
    let after = (
        state_of_run(&db, &id).await,
        attempts(&db, &id).await,
        db.scalar::<i64>("select claim_epoch from bot_runs").await,
        db.scalar::<i64>("select count(*) from bot_audit").await,
    );
    assert_eq!(
        before, after,
        "failed set check must leave all state untouched"
    );

    tx.commit().await.unwrap();
    assert!(matches!(RunStore::claim(&hasty).await, Err(Error::Stale)));
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    let again = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("new worker reclaims");
    assert_eq!(again.attempt.run, id);
    assert_eq!(attempts(&db, &id).await, 0);
}
