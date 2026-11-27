//! Unreadable due schedule rows (PG): corrupt cron or destination values are surfaced per row
//! in `DuePage::unreadable` without blocking sibling rows, stalling pagination, or mutating state.

use crate::support::{schedule::*, *};
use bot_core::{id::*, schedule::*, state::*};

const OWNER: &str = "local:owner";

async fn plant(db: &TestDb, origin: &RunContext, id: &str, now: i64, ago: i64, kind: &str) {
    let (at, cron) = match kind {
        "once" => (format!("{}", now - ago), "null, null".to_owned()),
        cron => ("null".to_owned(), format!("'{cron}', 'UTC'")),
    };
    let (cron_col, zone) = cron.split_once(", ").unwrap();
    db.sql(&format!(
        "insert into bot_user_schedules (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
                ceiling, at_ms, cron, timezone, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{OWNER}', 'main', '{}', 'c-{id}', 'desk', 'direct', 'c1',
                 '{{run,calendar.read}}', {at}, {cron_col}, {zone}, 'check', {}, {})",
        origin.attempt.run.0,
        now - ago - 3_600_000,
        now - ago
    ))
    .await;
}

fn ids(page: &DuePage) -> Vec<String> {
    page.schedules.iter().map(|s| s.id.0.clone()).collect()
}

fn pos(id: &str, at_ms: i64) -> DuePos {
    DuePos {
        at: at(at_ms),
        profile: profile_id(),
        id: ScheduleId(id.into()),
    }
}

/// Inserts an invalid cron, a valid one-shot, and a valid cron with an invalid timezone (validated by scheduler).
async fn three(db: &TestDb) -> i64 {
    let origin = run_in(db, "r1", OWNER, ConversationKind::Direct).await;
    db.sql("update bot_runs set state = 'succeeded', lease_until_ms = null, lease_owner = null, finished_ms = bot_now_ms()")
        .await;
    let now = db.now_ms().await;
    plant(db, &origin, "a-bad", now, 50_000, "not-a-cron").await;
    plant(db, &origin, "b-ok", now, 40_000, "once").await;
    plant(db, &origin, "c-zone", now, 30_000, "0 9 * * *").await;
    db.sql("update bot_user_schedules set timezone = 'Invalid/Zone' where id = 'c-zone'")
        .await;
    now
}

async fn fingerprint(db: &TestDb) -> String {
    db.scalar::<String>(
        "select coalesce(md5(string_agg(id || ':' || state || ':' || cursor_ms || ':' || coalesce(next_ms::text, '') || ':' || coalesce(close_reason, ''), ',' order by id)), '') from bot_user_schedules",
    )
    .await
}

#[tokio::test]
async fn an_unreadable_cron_is_reported_on_its_own_and_the_rest_of_the_page_is_read() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let now = three(&db).await;
    let before = fingerprint(&db).await;
    let audits = db.scalar::<i64>("select count(*) from bot_audit").await;
    let page = users(&db).due(None, 10).await.unwrap();
    assert_eq!(
        ids(&page),
        ["b-ok", "c-zone"],
        "valid rows are returned alongside unreadable ones"
    );
    assert_eq!(
        page.unreadable,
        [UnreadableSchedule {
            profile: profile_id(),
            id: ScheduleId("a-bad".into()),
            cursor: at(now - 50_000 - 3_600_000),
            at: at(now - 50_000),
            reason: CloseReason::InvalidTiming,
        }]
    );
    assert!(page.next.is_none());
    users(&db).due(None, 10).await.unwrap();
    assert_eq!(fingerprint(&db).await, before);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit").await,
        audits
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules where state = 'active'")
            .await,
        3
    );
}

#[tokio::test]
async fn the_page_size_and_the_resume_key_count_the_unreadable_rows_too_so_a_page_of_only_bad_rows_still_advances()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let now = three(&db).await;
    let (a, b, c) = (
        ("a-bad", now - 50_000),
        ("b-ok", now - 40_000),
        ("c-zone", now - 30_000),
    );
    let p1 = users(&db).due(None, 1).await.unwrap();
    assert!(p1.schedules.is_empty());
    assert_eq!(p1.unreadable.len(), 1);
    assert_eq!(
        p1.next,
        Some(DueKey {
            after: pos(a.0, a.1),
            end: pos(c.0, c.1)
        }),
        "unreadable rows consume page slots and advance the resume key"
    );
    let p2 = users(&db).due(p1.next, 1).await.unwrap();
    assert_eq!(
        (ids(&p2), p2.unreadable.len()),
        (vec!["b-ok".to_owned()], 0)
    );
    assert_eq!(
        p2.next,
        Some(DueKey {
            after: pos(b.0, b.1),
            end: pos(c.0, c.1)
        })
    );
    let p3 = users(&db).due(p2.next, 1).await.unwrap();
    assert_eq!(ids(&p3), ["c-zone"]);
    assert!(p3.next.is_none());

    let two = users(&db).due(None, 2).await.unwrap();
    assert_eq!(
        (ids(&two), two.unreadable.len(), two.next.is_some()),
        (vec!["b-ok".to_owned()], 1, true)
    );
}

#[tokio::test]
async fn closing_an_unreadable_row_leaves_the_others_and_is_recorded_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let now = three(&db).await;
    let bad = ScheduleId("a-bad".into());
    let close = |revision: Option<&str>| AdvanceSchedule {
        profile: profile_id(),
        id: bad.clone(),
        expected_cursor: at(now - 50_000 - 3_600_000),
        checked_revision: revision.map(|r| Revision(r.into())),
        next: NextOccurrence::Close(CloseReason::InvalidTiming),
    };
    users(&db).advance(close(Some("r1"))).await.unwrap();
    let row = async |id: &str| {
        db.scalar::<String>(&format!(
            "select state || ':' || coalesce(close_reason, '') from bot_user_schedules where id = '{id}'"
        ))
        .await
    };
    assert_eq!(row("a-bad").await, "closed:invalid_timing");
    assert_eq!(row("b-ok").await, "active:");
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_audit where action = 'schedule.close' and target = 'a-bad' and verdict = 'invalid_timing'"
        )
        .await,
        1
    );
    let page = users(&db).due(None, 10).await.unwrap();
    assert!(page.unreadable.is_empty());
    assert_eq!(ids(&page), ["b-ok", "c-zone"]);
    assert_eq!(users(&db).advance(close(Some("r1"))).await.unwrap(), None);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'schedule.close'")
            .await,
        1
    );
}

#[tokio::test]
async fn a_destination_kind_the_store_cannot_read_is_reported_per_row_with_its_own_reason() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    db.sql("update bot_runs set state = 'succeeded', lease_until_ms = null, lease_owner = null, finished_ms = bot_now_ms()")
        .await;
    let now = db.now_ms().await;
    plant(&db, &origin, "a-odd", now, 50_000, "once").await;
    plant(&db, &origin, "b-ok", now, 40_000, "once").await;
    plant(&db, &origin, "c-both", now, 30_000, "not-a-cron").await;
    for id in ["a-odd", "c-both"] {
        db.sql(&format!(
            "update bot_user_schedules set dest_binding = 'slack', dest_kind = 'weird', dest_id = 'C9' where id = '{id}'"
        ))
        .await;
    }
    let before = fingerprint(&db).await;
    let page = users(&db).due(None, 10).await.unwrap();
    assert_eq!(ids(&page), ["b-ok"]);
    assert_eq!(
        page.unreadable
            .iter()
            .map(|u| (u.id.0.as_str(), u.reason, u.at))
            .collect::<Vec<_>>(),
        [
            ("a-odd", CloseReason::InvalidDestination, at(now - 50_000)),
            // Timing errors take precedence when both timing and destination are corrupt.
            ("c-both", CloseReason::InvalidTiming, at(now - 30_000)),
        ]
    );
    assert_eq!(fingerprint(&db).await, before);
    assert_eq!(page.schedules[0].at, at(now - 40_000));

    let odd = ScheduleId("a-odd".into());
    let outbox = db.scalar::<i64>("select count(*) from bot_outbox").await;
    users(&db)
        .advance(AdvanceSchedule {
            profile: profile_id(),
            id: odd.clone(),
            expected_cursor: at(now - 50_000 - 3_600_000),
            checked_revision: Some(Revision("r1".into())),
            next: NextOccurrence::Close(CloseReason::InvalidDestination),
        })
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<String>(
            "select state || ':' || close_reason from bot_user_schedules where id = 'a-odd'"
        )
        .await,
        "closed:invalid_destination"
    );
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_audit where action = 'schedule.close' and target = 'a-odd' and verdict = 'invalid_destination'"
        )
        .await,
        1
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_outbox").await,
        outbox
    );
}
