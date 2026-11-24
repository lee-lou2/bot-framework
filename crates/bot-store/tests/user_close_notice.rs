//! 사용자 예약을 닫을 때의 안내(PG): 수신 경로가 없어져 닫은 예약만 원래 대화에 고정 안내 한 건을 같은 커밋으로 남기고, 같은 종료의 재전달은 안내를 더 쌓지 않는다.

mod support;

use bot_core::{
    id::*,
    port::AuditLog,
    schedule::{AdvanceSchedule, CloseReason, NextOccurrence},
    state::ConversationKind,
};
use support::{schedule::*, *};

const OWNER: &str = "local:owner";

fn closing(id: &ScheduleId, cursor: i64, reason: CloseReason) -> AdvanceSchedule {
    AdvanceSchedule {
        profile: profile_id(),
        id: id.clone(),
        expected_cursor: at(cursor),
        checked_revision: Some(Revision("r1".into())),
        next: NextOccurrence::Close(reason),
    }
}

async fn notices(db: &TestDb) -> Vec<(String, String, String, String, Option<String>)> {
    sqlx::query_as(
        "select body, principal, conv_kind, conv_id, thread from bot_outbox where run = (select origin_run from bot_user_schedules) order by seq",
    )
    .fetch_all(db.store.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn an_unrouted_close_leaves_one_fixed_notice_in_the_original_conversation_and_a_repeat_adds_none()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let id = make(&db, &origin, "c1", spec_in(&db, 3_600_000).await)
        .await
        .unwrap();
    let cursor = column::<i64>(&db, &id, "cursor_ms").await;
    let before = notices(&db).await.len();
    assert!(
        users(&db)
            .advance(closing(&id, cursor, CloseReason::Unrouted))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(column::<String>(&db, &id, "state").await, "closed");
    assert_eq!(column::<String>(&db, &id, "close_reason").await, "unrouted");
    // 원래 대화(c1)에 고정 안내 한 건: 수신자·예약 내용이 없다
    let after = notices(&db).await;
    assert_eq!(after.len(), before + 1);
    let (body, principal, kind, conv, _) = after.last().unwrap().clone();
    assert_eq!(
        body,
        format!(
            r#"{{"text":"{}"}}"#,
            CloseReason::Unrouted.notice().unwrap()
        )
    );
    assert_eq!(
        (principal.as_str(), kind.as_str(), conv.as_str()),
        (OWNER, "direct", "c1")
    );
    assert!(!body.contains("일정을 확인해 줘"));
    // 같은 종료를 다시 받아도 감사·안내는 그대로다
    assert!(
        users(&db)
            .advance(closing(&id, cursor, CloseReason::Unrouted))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(notices(&db).await.len(), before + 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'schedule.close'")
            .await,
        1
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn other_close_reasons_and_non_push_conversations_leave_no_notice() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let id = make(&db, &origin, "c1", spec_in(&db, 3_600_000).await)
        .await
        .unwrap();
    let cursor = column::<i64>(&db, &id, "cursor_ms").await;
    let before = notices(&db).await.len();
    users(&db)
        .advance(closing(&id, cursor, CloseReason::MemberRevoked))
        .await
        .unwrap();
    assert_eq!(
        column::<String>(&db, &id, "close_reason").await,
        "member_revoked"
    );
    assert_eq!(
        notices(&db).await.len(),
        before,
        "안내는 수신 경로가 없어진 종료에만 있다"
    );

    // CLI에서 만든 예약은 안내를 보낼 대화가 없다
    let cli = run_in(&db, "r2", OWNER, ConversationKind::Cli).await;
    let other = make(&db, &cli, "c2", spec_in(&db, 3_600_000).await)
        .await
        .unwrap();
    let cursor = column::<i64>(&db, &other, "cursor_ms").await;
    users(&db)
        .advance(closing(&other, cursor, CloseReason::Unrouted))
        .await
        .unwrap();
    assert_eq!(
        column::<String>(&db, &other, "close_reason").await,
        "unrouted"
    );
    assert_eq!(
        db.scalar::<i64>(&format!(
            "select count(*) from bot_outbox where run = (select origin_run from bot_user_schedules where id = '{}')",
            other.0
        ))
        .await,
        0
    );
}
