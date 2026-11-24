//! 사용자 예약의 닫기 사유(PG): 닫는 결정도 발화처럼 **읽은 커서**를 기준으로 한다. 낡은 커서로 닫으려 하면 `Stale`이고 감사·안내는 0이다.
//! 이미 닫힌 예약을 다시 닫아도 처음 사유와 기록이 그대로다. 마지막 발생을 접수하면 같은 커밋에서 `finished`로 닫히고 감사 한 줄이 남는다(안내 없음).
//! 사유마다 저장 열과 감사 낱말이 `CloseReason::code()`이고, 원래 대화 안내는 수신 경로가 없어진 종료(`Unrouted`)에만 있다.

mod support;

use bot_core::{
    id::*,
    port::{AuditLog, Error},
    schedule::{AdvanceSchedule, CloseReason, NextOccurrence},
    state::ConversationKind,
};
use std::collections::BTreeSet;
use support::{schedule::*, *};

const OWNER: &str = "local:owner";

const ALL: [CloseReason; 9] = [
    CloseReason::Finished,
    CloseReason::MemberRevoked,
    CloseReason::BindingRevoked,
    CloseReason::AgentRevoked,
    CloseReason::PermissionRevoked,
    CloseReason::InvalidTiming,
    CloseReason::InvalidDestination,
    CloseReason::Unrouted,
    CloseReason::ProfileInactive,
];

fn advance(id: &ScheduleId, cursor: i64, next: NextOccurrence) -> AdvanceSchedule {
    AdvanceSchedule {
        profile: profile_id(),
        id: id.clone(),
        expected_cursor: at(cursor),
        checked_revision: Some(Revision("r1".into())),
        next,
    }
}

fn close(id: &ScheduleId, cursor: i64, reason: CloseReason) -> AdvanceSchedule {
    let mut command = advance(id, cursor, NextOccurrence::Close(reason));
    // 비활성 프로필의 닫기만 판본 없이 받고, 판본을 싣고는 받지 않는다
    if reason == CloseReason::ProfileInactive {
        command.checked_revision = None;
    }
    command
}

fn fire(at_ms: i64, next: Option<i64>) -> NextOccurrence {
    NextOccurrence::Fire {
        at: at(at_ms),
        next: next.map(at),
        scopes: BTreeSet::from(["run".to_owned()]),
    }
}

/// 이 예약을 만든 run의 outbox 행 수(원래 대화 안내가 쌓이는 곳).
async fn notices(db: &TestDb, id: &ScheduleId) -> i64 {
    db.scalar(&format!(
        "select count(*) from bot_outbox where run = (select origin_run from bot_user_schedules where id = '{}')",
        id.0
    ))
    .await
}

async fn close_audits(db: &TestDb, id: &ScheduleId) -> Vec<String> {
    sqlx::query_scalar(&format!(
        "select verdict from bot_audit where action = 'schedule.close' and target = '{}' order by seq",
        id.0
    ))
    .fetch_all(db.store.pool())
    .await
    .unwrap()
}

/// 소유자의 DM에서 만든 한 번짜리 예약을 지난 시각으로 옮겨 둔다. (예약, 예정 시각, 커서)
async fn one_shot_due(db: &TestDb, request: &str) -> (ScheduleId, i64, i64) {
    let origin = run_in(db, request, OWNER, ConversationKind::Direct).await;
    let id = make(db, &origin, "c1", spec_in(db, 3_600_000).await)
        .await
        .unwrap();
    make_due(db, &id, 5_000).await;
    (
        id.clone(),
        column::<i64>(db, &id, "at_ms").await,
        column::<i64>(db, &id, "cursor_ms").await,
    )
}

#[tokio::test]
async fn a_close_decided_from_a_stale_view_is_refused_with_no_audit_and_no_notice() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let id = make(&db, &origin, "c1", spec_cron("*/5 * * * *", "UTC"))
        .await
        .unwrap();
    make_due(&db, &id, 5_000).await;
    let stale = column::<i64>(&db, &id, "cursor_ms").await;
    let due = column::<i64>(&db, &id, "next_ms").await;
    // 다른 스케줄러가 먼저 이 발생을 접수해 커서가 움직였다
    let later = db.now_ms().await + 3_600_000;
    users(&db)
        .advance(advance(&id, stale, fire(due, Some(later))))
        .await
        .unwrap()
        .expect("접수한 run");
    let moved = column::<i64>(&db, &id, "cursor_ms").await;
    assert_ne!(moved, stale);
    let (notices_before, audits_before) = (
        notices(&db, &id).await,
        db.scalar::<i64>("select count(*) from bot_audit").await,
    );
    // 옛 커서를 읽고 내린 닫기 결정은 받지 않는다(안내가 있는 사유도 같다)
    for reason in [CloseReason::Unrouted, CloseReason::MemberRevoked] {
        let refused = users(&db).advance(close(&id, stale, reason)).await;
        assert!(
            matches!(refused, Err(Error::Stale)),
            "{reason:?}: {refused:?}"
        );
    }
    assert_eq!(column::<String>(&db, &id, "state").await, "active");
    assert_eq!(
        column::<Option<String>>(&db, &id, "close_reason").await,
        None
    );
    assert_eq!(column::<i64>(&db, &id, "next_ms").await, later);
    assert_eq!(column::<i64>(&db, &id, "cursor_ms").await, moved);
    assert_eq!(notices(&db, &id).await, notices_before, "안내 0");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit").await,
        audits_before,
        "감사 0"
    );
    assert!(close_audits(&db, &id).await.is_empty());
    // 지금 커서를 읽고 내린 결정은 받는다: 사유 낱말·안내 한 건·감사 한 줄
    assert!(
        users(&db)
            .advance(close(&id, moved, CloseReason::Unrouted))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(column::<String>(&db, &id, "state").await, "closed");
    assert_eq!(column::<String>(&db, &id, "close_reason").await, "unrouted");
    assert_eq!(notices(&db, &id).await, notices_before + 1);
    assert_eq!(close_audits(&db, &id).await, ["unrouted"]);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn closing_again_keeps_the_first_reason_and_writes_nothing_more() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (id, _, cursor) = one_shot_due(&db, "r1").await;
    users(&db)
        .advance(close(&id, cursor, CloseReason::Unrouted))
        .await
        .unwrap();
    let (notices_before, audits_before) = (
        notices(&db, &id).await,
        db.scalar::<i64>("select count(*) from bot_audit").await,
    );
    // 이미 닫힌 예약: 같은 사유·다른 사유·옛 커서 모두 받은 것으로 치고(`None`) 아무것도 쓰지 않는다
    for (stale, reason) in [
        (cursor, CloseReason::Unrouted),
        (cursor, CloseReason::MemberRevoked),
        (cursor - 1, CloseReason::Unrouted),
    ] {
        assert!(
            users(&db)
                .advance(close(&id, stale, reason))
                .await
                .unwrap()
                .is_none(),
            "{reason:?}"
        );
    }
    assert_eq!(column::<String>(&db, &id, "close_reason").await, "unrouted");
    assert_eq!(notices(&db, &id).await, notices_before);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit").await,
        audits_before
    );
    assert_eq!(close_audits(&db, &id).await, ["unrouted"]);
}

#[tokio::test]
async fn every_close_reason_is_stored_and_audited_by_its_code_and_only_unrouted_leaves_a_notice() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for (n, reason) in ALL.into_iter().enumerate() {
        let (id, _, cursor) = one_shot_due(&db, &format!("r{n}")).await;
        let before = notices(&db, &id).await;
        if reason == CloseReason::ProfileInactive {
            // 정말 비활성인 프로필만 판본 없이 닫는다(마지막 사유라 뒤에 만들 예약은 없다)
            db.sql("delete from bot_active_profiles where profile = 'personal'")
                .await;
        }
        users(&db)
            .advance(close(&id, cursor, reason))
            .await
            .unwrap();
        assert_eq!(
            column::<String>(&db, &id, "state").await,
            "closed",
            "{reason:?}"
        );
        assert_eq!(
            column::<String>(&db, &id, "close_reason").await,
            reason.code(),
            "{reason:?}"
        );
        assert_eq!(close_audits(&db, &id).await, [reason.code()], "{reason:?}");
        assert_eq!(
            notices(&db, &id).await,
            before + i64::from(reason == CloseReason::Unrouted),
            "{reason:?}: 안내는 수신 경로가 없어진 종료에만 있다"
        );
    }
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn the_last_occurrence_closes_the_schedule_as_finished_with_one_audit_and_no_notice() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (id, at_ms, cursor) = one_shot_due(&db, "r1").await;
    let before = notices(&db, &id).await;
    let run = users(&db)
        .advance(advance(&id, cursor, fire(at_ms, None)))
        .await
        .unwrap()
        .expect("접수한 run");
    assert_eq!(column::<String>(&db, &id, "state").await, "closed");
    assert_eq!(column::<String>(&db, &id, "close_reason").await, "finished");
    assert_eq!(close_audits(&db, &id).await, ["finished"]);
    assert_eq!(notices(&db, &id).await, before, "정상 종료에는 안내가 없다");
    assert_eq!(
        db.scalar::<String>(&format!(
            "select actor from bot_audit where action = 'schedule.close' and target = '{}'",
            id.0
        ))
        .await,
        "system"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'schedule.fire'")
            .await,
        1
    );
    // 같은 발생을 다시 접수해도 같은 run이고 닫기 감사는 늘지 않는다
    let again = users(&db)
        .advance(advance(&id, cursor, fire(at_ms, None)))
        .await
        .unwrap();
    assert_eq!(again, Some(run));
    assert_eq!(close_audits(&db, &id).await, ["finished"]);
    assert_eq!(notices(&db, &id).await, before);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn an_occurrence_that_is_not_the_last_writes_no_close_audit_and_the_schedule_stays_active() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let id = make(&db, &origin, "c1", spec_cron("*/5 * * * *", "UTC"))
        .await
        .unwrap();
    make_due(&db, &id, 5_000).await;
    let cursor = column::<i64>(&db, &id, "cursor_ms").await;
    let due = column::<i64>(&db, &id, "next_ms").await;
    let later = db.now_ms().await + 3_600_000;
    users(&db)
        .advance(advance(&id, cursor, fire(due, Some(later))))
        .await
        .unwrap()
        .expect("접수한 run");
    assert_eq!(column::<String>(&db, &id, "state").await, "active");
    assert_eq!(
        column::<Option<String>>(&db, &id, "close_reason").await,
        None
    );
    assert!(close_audits(&db, &id).await.is_empty());
}
