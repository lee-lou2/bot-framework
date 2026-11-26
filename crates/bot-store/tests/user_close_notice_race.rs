//! 닫기 안내와 같은 run의 일반 회신이 동시에 원래 대화 outbox에 들어와도 순번이 겹치지 않는다: 두 쓰기 경로가 같은 run 잠금 아래에서 순번을 정한다.
//! 닫는 쪽은 예약 행이 아니라 run 행을 먼저 기다린다(run → 예약 순서): 그 사이 같은 run을 잠근 취소 호출이 예약 행을 잠글 수 있고 서로 기다리지 않는다.

mod support;

use bot_core::{
    id::*,
    schedule::{AdvanceSchedule, CloseReason, NextOccurrence, UserSchedules},
    state::ConversationKind,
};
use std::time::Duration;
use support::{schedule::*, *};

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

    // 일반 회신을 쓰는 쪽이 run을 잠갔고 아직 커밋하지 않았다
    let mut other = db.raw().await;
    sqlx::query("begin").execute(&mut other).await.unwrap();
    sqlx::query(&format!(
        "select 1 from bot_runs where id = '{run}' for update"
    ))
    .fetch_all(&mut other)
    .await
    .unwrap();

    // 그 사이 닫는 쪽이 들어온다
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
    // 닫는 쪽이 run 잠금을 기다리는 중임을 확인한다(기다리지 않고 지나갔다면 이 시험은 아무것도 보지 못한다)
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
        "닫는 쪽은 같은 run의 일반 회신이 끝나기를 기다려야 한다"
    );

    // 기다리는 닫는 쪽이 예약 행을 쥐고 있지 않다: 같은 run을 잠근 취소 호출이 예약 행을 잠그러 가도 교착하지 않는다
    sqlx::query("set local lock_timeout = '3s'")
        .execute(&mut other)
        .await
        .unwrap();
    sqlx::query(&format!(
        "select 1 from bot_user_schedules where id = '{schedule_id}' for update"
    ))
    .fetch_all(&mut other)
    .await
    .expect("닫는 쪽이 예약 행을 쥔 채 run을 기다린다");
    // 일반 회신: 다음 순번에 쓰고 커밋한다
    sqlx::query(&format!(
        "insert into bot_outbox (profile, run, seq, call, binding, principal, conv_kind, conv_id, body, transport)
         select 'personal', '{run}', n.seq, 'reply:{run}:' || n.seq::text, 'desk', '{OWNER}', 'direct', 'c1', '{{\"text\":\"답\"}}', 'fixture-push'
         from (select coalesce(max(seq) + 1, 0) as seq from bot_outbox where run = '{run}') n"
    ))
    .execute(&mut other)
    .await
    .unwrap();
    sqlx::query("commit").execute(&mut other).await.unwrap();

    // 닫는 쪽은 처음부터 성공한다(재시도 없이): 안내는 일반 회신 다음 순번이다
    let closed = task.await.unwrap();
    assert!(closed.expect("첫 시도에 닫힌다").is_none());
    let seqs: Vec<i32> = sqlx::query_scalar(&format!(
        "select seq from bot_outbox where run = '{run}' order by seq"
    ))
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    let expected: Vec<i32> = (0..i32::try_from(rows_before).unwrap() + 2).collect();
    assert_eq!(seqs, expected, "순번이 이어지고 겹치지 않는다");
    let (last, transport): (String, String) = sqlx::query_as(&format!(
        "select body, transport from bot_outbox where run = '{run}' order by seq desc limit 1"
    ))
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert!(last.contains("멈췄어요"), "{last}");
    assert_eq!(transport, "fixture-push");
    assert_eq!(column::<String>(&db, &id, "state").await, "closed");
}
