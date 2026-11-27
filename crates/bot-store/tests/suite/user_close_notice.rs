//! User schedule close notices (PG): only `Unrouted` closures enqueue one fixed notice in the
//! origin push conversation, idempotently, and hold cleanly when `reply_transport` is missing.

use crate::support::{schedule::*, *};
use bot_core::{
    id::*,
    port::AuditLog,
    schedule::{AdvanceSchedule, CloseReason, NextOccurrence},
    state::ConversationKind,
};

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
    let spec = spec_in(&db, 3_600_000).await;
    let instruction = spec.instruction.clone();
    let id = make(&db, &origin, "c1", spec).await.unwrap();
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
    assert!(!body.contains(&instruction));

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
async fn null_origin_transport_refuses_push_schedule_fire_and_holds_close_notice() {
    use bot_core::{
        admin::{AdminRead, HoldKind},
        port::Error,
    };
    use std::collections::BTreeSet;

    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r-null", OWNER, ConversationKind::Direct).await;
    let id = make(&db, &origin, "c1", spec_in(&db, 60_000).await)
        .await
        .unwrap();
    // Simulate a legacy due schedule whose origin run has `reply_transport IS NULL`.
    sqlx::query(
        "update bot_user_schedules
         set cursor_ms = cursor_ms - 5000, at_ms = cursor_ms - 1000, next_ms = cursor_ms - 1000
         where profile = 'personal' and id = $1",
    )
    .bind(&id.0)
    .execute(db.store.pool())
    .await
    .unwrap();
    sqlx::query(
        "update bot_runs set reply_transport = null where profile = 'personal' and id = $1",
    )
    .bind(&origin.attempt.run.0)
    .execute(db.store.pool())
    .await
    .unwrap();

    let cursor = column::<i64>(&db, &id, "cursor_ms").await;
    let fire_at = column::<i64>(&db, &id, "at_ms").await;
    let runs_before = db.scalar::<i64>("select count(*) from bot_runs").await;
    assert!(matches!(
        users(&db)
            .advance(AdvanceSchedule {
                profile: profile_id(),
                id: id.clone(),
                expected_cursor: at(cursor),
                checked_revision: Some(Revision("r1".into())),
                next: NextOccurrence::Fire {
                    at: at(fire_at),
                    next: None,
                    scopes: BTreeSet::new(),
                },
            })
            .await,
        Err(Error::Invalid)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs").await,
        runs_before
    );
    // The documented diagnostic query finds active push schedules missing `reply_transport`.
    let found: Vec<(String, String, String, String)> = sqlx::query_as(
        "select s.profile, s.id, s.origin_run, s.origin_kind
         from bot_user_schedules s
         join bot_runs r on (r.profile, r.id) = (s.profile, s.origin_run)
         where s.state = 'active'
           and s.origin_kind in ('direct', 'channel')
           and r.reply_transport is null",
    )
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        found,
        [(
            "personal".to_owned(),
            id.0.clone(),
            origin.attempt.run.0.clone(),
            "direct".to_owned(),
        )]
    );

    // Closing still succeeds and records the unsendable notice directly as `held`.
    assert!(
        users(&db)
            .advance(closing(&id, cursor, CloseReason::Unrouted))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(column::<String>(&db, &id, "state").await, "closed");
    let row: (String, Option<String>, String) = sqlx::query_as(
        "select state, reason, transport from bot_outbox where profile = 'personal' and run = $1",
    )
    .bind(&origin.attempt.run.0)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "held".to_owned(),
            Some("missing_transport".to_owned()),
            String::new()
        )
    );
    let holds = AdminRead::holds(
        &db.store,
        &crate::support::admin::operator(),
        Some(profile_id()),
    )
    .await
    .unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].kind, HoldKind::Reply);
    assert_eq!(holds[0].reason, "missing_transport");
    assert!(holds[0].hold_id.is_some());
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
        "only Unrouted closures enqueue an origin notice"
    );

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
