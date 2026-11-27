use super::*;

use bot_store::Store;
use sqlx::{Postgres, Transaction};

async fn lock_audit(db: &TestDb) -> (Transaction<'static, Postgres>, i32) {
    let mut tx = db.store.pool().begin().await.unwrap();
    let pid = sqlx::query_scalar(
        "select pg_backend_pid() from bot_audit_head where singleton for update",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    (tx, pid)
}

async fn give(store: &Store, parent: &RunContext, name: &str) -> RunId {
    store
        .delegate(delegate(
            &parent.attempt,
            name,
            &format!("{}/{name}", parent.agent.0),
            "research",
            &["run"],
        ))
        .await
        .unwrap()
}

#[tokio::test]
async fn cancelling_rescans_when_a_new_child_delegates_during_the_first_scan() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // Locking root blocks FK checks on all new children, so cancel a non-root subtree instead.
    let root = leased(&db, "root").await;
    give(&db.store, &root, "a").await;
    let parent = db.store.claim().await.unwrap().unwrap();
    give(&db.store, &parent, "b").await;
    let target = db.store.claim().await.unwrap().unwrap();
    let prior = give(&db.store, &target, "prior").await;
    let prior_at = db.store.claim().await.unwrap().unwrap().attempt;
    assert_eq!(prior, prior_at.run);
    db.store
        .finish(prior_at, done("prior result"))
        .await
        .unwrap();
    let target = db.store.claim().await.unwrap().unwrap();

    // Hold the first scan on both the in-flight new child and an existing child row.
    let mut prior_lock = db.store.pool().begin().await.unwrap();
    let prior_pid: i32 = sqlx::query_scalar(
        "select pg_backend_pid() from bot_runs where profile = 'personal' and id = $1 for update",
    )
    .bind(&prior.0)
    .fetch_one(&mut *prior_lock)
    .await
    .unwrap();
    let (audit, audit_pid) = lock_audit(&db).await;
    let creator = db.peer("first-helper").await;
    let target_run = target.attempt.run.clone();
    let giving = tokio::spawn(async move { give(&creator, &target, "new").await });
    let giving_pid = db.waiter_blocked_by(audit_pid).await;
    let admin = db.peer("admin").await;
    let cancelling_run = target_run.clone();
    let cancelling = tokio::spawn(async move {
        admin
            .cancel(
                profile(),
                cancelling_run,
                PrincipalId("operator".into()),
                "stop".into(),
            )
            .await
            .unwrap();
    });
    let cancel_pid = db.waiter_blocked_by(giving_pid).await;
    audit.commit().await.unwrap();
    let child = giving.await.unwrap();
    assert_eq!(db.waiter_blocked_by(prior_pid).await, cancel_pid);

    // On the second snapshot, the child is visible while a newly delegated grandchild is still uncommitted.
    let claimed = db.store.claim().await.unwrap().unwrap();
    assert_eq!(claimed.attempt.run, child);
    let (audit, audit_pid) = lock_audit(&db).await;
    let creator = db.peer("second-helper").await;
    let giving = tokio::spawn(async move { give(&creator, &claimed, "new").await });
    let giving_pid = db.waiter_blocked_by(audit_pid).await;
    prior_lock.commit().await.unwrap();
    assert_eq!(db.waiter_blocked_by(giving_pid).await, cancel_pid);
    audit.commit().await.unwrap();
    let grandchild = giving.await.unwrap();
    cancelling.await.unwrap();

    for run in [&target_run, &child, &grandchild] {
        assert_eq!(state_of(&db, run).await, "cancelled");
    }
    assert_eq!(state_of(&db, &prior).await, "succeeded");
}
