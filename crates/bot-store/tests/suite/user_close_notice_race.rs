//! Concurrent schedule close notice and regular reply serialize on the run lock (`run -> schedule` order) without sequence collisions or deadlocks.

use crate::support::{schedule::*, *};
use bot_core::{
    id::*,
    schedule::{AdvanceSchedule, CloseReason, NextOccurrence, UserSchedules},
    state::ConversationKind,
};
use std::time::Duration;

const OWNER: &str = "local:owner";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_close_notice_takes_the_next_sequence_after_a_concurrent_reply_and_never_holds_the_schedule_while_waiting()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    db.sql("update bot_runs set reply_transport = 'fixture-push'")
        .await;
    let id = make(&db, &origin, "c1", spec_in(&db, 3_600_000).await)
        .await
        .unwrap();
    let cursor = column::<i64>(&db, &id, "cursor_ms").await;
    let run = origin.attempt.run.0.clone();
    let rows_before = db
        .scalar::<i64>(&format!(
            "select count(*) from bot_outbox where run = '{run}'"
        ))
        .await;

    // Hold the run lock as an in-flight regular reply writer.
    let mut other = db.raw().await;
    sqlx::query("begin").execute(&mut other).await.unwrap();
    sqlx::query(&format!(
        "select 1 from bot_runs where id = '{run}' for update"
    ))
    .fetch_all(&mut other)
    .await
    .unwrap();

    let closer = db.peer("closer").await;
    let (closing_id, schedule_id) = (id.clone(), id.0.clone());
    let task = tokio::spawn(async move {
        closer
            .advance(AdvanceSchedule {
                profile: profile_id(),
                id: closing_id,
                expected_cursor: at(cursor),
                checked_revision: Some(Revision("r1".into())),
                next: NextOccurrence::Close(CloseReason::Unrouted),
            })
            .await
    });
    let mut waiting = false;
    for _ in 0..100 {
        let n = db
            .scalar::<i64>(&format!(
                "select count(*) from pg_stat_activity where application_name = '{}' and wait_event_type = 'Lock'",
                db.schema
            ))
            .await;
        if n >= 1 {
            waiting = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        waiting,
        "closer must wait for concurrent reply holding the run lock"
    );

    // Closer must acquire run before schedule so a run-holding cancel can lock the schedule without deadlock.
    sqlx::query("set local lock_timeout = '3s'")
        .execute(&mut other)
        .await
        .unwrap();
    sqlx::query(&format!(
        "select 1 from bot_user_schedules where id = '{schedule_id}' for update"
    ))
    .fetch_all(&mut other)
    .await
    .expect("closer must not hold schedule lock while waiting on run");
    sqlx::query(&format!(
        "insert into bot_outbox (profile, run, seq, call, binding, principal, conv_kind, conv_id, body, transport)
         select 'personal', '{run}', n.seq, 'reply:{run}:' || n.seq::text, 'desk', '{OWNER}', 'direct', 'c1', '{{\"text\":\"reply\"}}', 'fixture-push'
         from (select coalesce(max(seq) + 1, 0) as seq from bot_outbox where run = '{run}') n"
    ))
    .execute(&mut other)
    .await
    .unwrap();
    sqlx::query("commit").execute(&mut other).await.unwrap();

    let closed = task.await.unwrap();
    assert!(closed.expect("close should succeed on first try").is_none());
    let seqs: Vec<i32> = sqlx::query_scalar(&format!(
        "select seq from bot_outbox where run = '{run}' order by seq"
    ))
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    let expected: Vec<i32> = (0..i32::try_from(rows_before).unwrap() + 2).collect();
    assert_eq!(seqs, expected, "outbox sequences must be contiguous");
    let (last, transport): (String, String) = sqlx::query_as(&format!(
        "select body, transport from bot_outbox where run = '{run}' order by seq desc limit 1"
    ))
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert!(
        last.contains(CloseReason::Unrouted.notice().unwrap()),
        "{last}"
    );
    assert_eq!(transport, "fixture-push");
    assert_eq!(column::<String>(&db, &id, "state").await, "closed");
}
