use super::*;

#[tokio::test]
async fn renew_after_waiting_past_expiry_is_stale() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "heartbeat").await;
    // Advance the test-schema DB clock while holding the row lock.
    db.sql("create table test_clock (ms bigint not null)").await;
    db.sql("insert into test_clock values (1000)").await;
    db.sql("create or replace function bot_now_ms() returns bigint language sql volatile as 'select ms from test_clock'").await;
    db.sql("update bot_runs set lease_until_ms = 1100").await;

    let mut tx = db.store.pool().begin().await.unwrap();
    let locker: i32 = sqlx::query_scalar("select pg_backend_pid() from bot_runs for update")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let peer = db.peer("heartbeat").await;
    let old = run.attempt.clone();
    let renewing = tokio::spawn(async move { peer.renew(old).await });
    db.waiter_blocked_by(locker).await;
    db.sql("update test_clock set ms = 1101").await;
    tx.commit().await.unwrap();

    assert!(stale(renewing.await.unwrap()));
    assert_eq!(
        db.scalar::<i64>("select lease_until_ms from bot_runs")
            .await,
        1100
    );
    let current = db.store.claim().await.unwrap().unwrap().attempt;
    assert_ne!(current, run.attempt);
    assert!(stale(db.store.renew(run.attempt).await));

    db.sql("update bot_runs set lease_until_ms = bot_now_ms()")
        .await;
    assert!(
        !db.store
            .renew(current.clone())
            .await
            .unwrap()
            .cancel_requested
    );
    db.store
        .cancel(
            current.profile.clone(),
            current.run.clone(),
            PrincipalId("operator".into()),
            "stop".into(),
        )
        .await
        .unwrap();
    assert!(db.store.renew(current).await.unwrap().cancel_requested);
}
