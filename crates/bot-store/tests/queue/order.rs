use super::*;

#[tokio::test]
async fn upgrade_backfills_only_legacy_public_ready_runs() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ready = accepted(&db, "legacy").await;
    let delayed = accepted(&db, "delayed").await;
    let private = accepted(&db, "private").await;
    for sql in [
        "drop index bot_runs_eligible",
        "drop trigger bot_runs_enter_ready on bot_runs",
        "drop function bot_runs_enter_ready()",
        "alter table bot_runs alter column not_before_ms set default 0",
        "delete from _sqlx_migrations where version = 24",
        "update bot_runs set created_ms = 25, not_before_ms = 0",
    ] {
        db.sql(sql).await;
    }
    sqlx::query("update bot_runs set not_before_ms = 1234 where id = $1")
        .bind(&delayed.0)
        .execute(db.store.pool())
        .await
        .unwrap();
    sqlx::query("update bot_runs set private = true where id = $1")
        .bind(&private.0)
        .execute(db.store.pool())
        .await
        .unwrap();
    db.store.migrate().await.unwrap();
    for (run, expected) in [(ready, 25_i64), (delayed, 1234), (private, 0)] {
        assert_eq!(
            db.scalar::<i64>(&format!(
                "select not_before_ms from bot_runs where id = '{}'",
                run.0
            ))
            .await,
            expected
        );
    }
}

#[tokio::test]
async fn entering_ready_records_immediate_arrival_and_preserves_a_future_delay() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.sql("create or replace function bot_now_ms() returns bigint language sql volatile as 'select 10000::bigint'").await;
    for (state, wait, lease, timestamp, expected) in [
        ("waiting", "'hold'", "null", 0, 10000_i64),
        ("leased", "null", "11000", 1, 10000),
        ("dead", "null", "null", 10000, 10000),
        ("retry", "null", "null", 11000, 11000),
    ] {
        let run = accepted(&db, state).await;
        db.sql(&format!(
            "update bot_runs set state = '{state}', wait_kind = {wait}, lease_until_ms = {lease}
             where id = '{}'",
            run.0
        ))
        .await;
        db.sql(&format!(
            "update bot_runs set state = 'ready', wait_kind = null, lease_until_ms = null, not_before_ms = {timestamp}
             where id = '{}'",
            run.0
        ))
        .await;
        assert_eq!(
            db.scalar::<i64>(&format!(
                "select not_before_ms from bot_runs where id = '{}'",
                run.0
            ))
            .await,
            expected,
            "{state} → ready"
        );
    }
}

#[tokio::test]
async fn an_expired_lease_precedes_a_new_ready_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let old = leased(&db, "old").await;
    db.sql("update bot_runs set lease_until_ms = bot_now_ms() - 1000 where state = 'leased'")
        .await;
    let fresh = accepted(&db, "fresh").await;

    let takeover = db.store.claim().await.unwrap().unwrap();
    assert_eq!(takeover.attempt.run, old.attempt.run);
    assert_ne!(takeover.attempt, old.attempt);
    assert_eq!(db.store.claim().await.unwrap().unwrap().attempt.run, fresh);
}

#[tokio::test]
async fn pick_uses_the_eligibility_expression_as_an_index_bound() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut tx = db.store.pool().begin().await.unwrap();
    sqlx::query("set local enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();
    let explain = concat!("explain ", include_str!("../../src/queue/pick.sql"));
    let plan: Vec<String> = sqlx::query_scalar(explain)
        .bind(Vec::<String>::new())
        .bind(Vec::<String>::new())
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    let plan = plan.join("\n");
    assert!(
        plan.lines()
            .skip_while(|line| !line.contains("Index Scan using bot_runs_eligible on bot_runs r"))
            .nth(1)
            .is_some_and(|line| line.contains("Index Cond:") && line.contains("<= ")),
        "{plan}"
    );
}
