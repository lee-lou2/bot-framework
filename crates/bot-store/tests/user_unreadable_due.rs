//! 저장된 시각 정의(cron)를 읽지 못한 예약의 읽기(PG): 한 행의 손상이 쪽 전체를 막지 않는다.
//! - `due`는 읽을 수 없는 행을 `unreadable`(이름표 + 읽은 커서)로 따로 돌려주고, 나머지는 그대로 한 쪽에 담는다.
//! - 쪽 크기(`limit`)와 이어 읽을 자리(`next`)는 정상 + 손상 행을 합친 원래 쪽을 기준으로 한다 — 손상 행만 있는 쪽도 앞으로 간다.
//! - 읽기는 행을 건드리지 않고(상태 · 커서 · 다음 확인 시각 그대로), 감사도 남기지 않는다. 닫는 것은 스케줄러의 `advance(Close)`다.
//! - 읽기 장애는 손상으로 다루지 않는다(이 파일은 저장된 값이 읽히지 않는 경우만 본다).

mod support;

use bot_core::{id::*, schedule::*, state::*};
use support::{schedule::*, *};

const OWNER: &str = "local:owner";

/// 다음 확인 시각이 지금보다 `ago`ms 전인 예약 한 행: `kind`가 정한 시각 정의. 커서는 그보다 한 시간 앞이다.
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
                 '{{run,calendar.read}}', {at}, {cron_col}, {zone}, '확인', {}, {})",
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

/// 세 행: 읽을 수 없는 cron(`not-a-cron`, 가장 먼저 도래), 정상 한 번짜리, 읽을 수 있는 cron + 해석할 수 없는 시간대(스케줄러가 판단할 일이라 저장소는 읽는다).
async fn three(db: &TestDb) -> i64 {
    let origin = run_in(db, "r1", OWNER, ConversationKind::Direct).await;
    db.sql("update bot_runs set state = 'succeeded', lease_until_ms = null, lease_owner = null, finished_ms = bot_now_ms()")
        .await;
    let now = db.now_ms().await;
    plant(db, &origin, "a-bad", now, 50_000, "not-a-cron").await;
    plant(db, &origin, "b-ok", now, 40_000, "once").await;
    // 시간대가 틀린 cron: 열은 `timezone`이라 `UTC` 대신 틀린 이름을 넣는다
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
        "손상 행은 빠지고 나머지는 그대로"
    );
    assert_eq!(
        page.unreadable,
        [UnreadableSchedule {
            profile: profile_id(),
            id: ScheduleId("a-bad".into()),
            cursor: at(now - 50_000 - 3_600_000),
            at: at(now - 50_000),
            reason: CloseReason::InvalidTiming,
        }],
        "이름표와 읽은 커서"
    );
    assert!(page.next.is_none());
    // 읽기는 행을 건드리지 않고 감사도 남기지 않는다(몇 번을 읽어도 같다)
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
    // 한도 1: 첫 쪽은 손상 행 하나뿐이다(정상 행 0). 그래도 이어 읽을 자리는 앞으로 간다
    let p1 = users(&db).due(None, 1).await.unwrap();
    assert!(p1.schedules.is_empty());
    assert_eq!(p1.unreadable.len(), 1);
    assert_eq!(
        p1.next,
        Some(DueKey {
            after: pos(a.0, a.1),
            end: pos(c.0, c.1)
        }),
        "손상 행도 한 칸을 차지한다"
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
    assert!(p3.next.is_none(), "끝 행까지 읽었다 = 한 바퀴 끝");
    // 한도 2: 손상 행 + 정상 행이 한 쪽에 같이 온다
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
    // 닫힌 행은 더 이상 읽히지 않고, 같은 종료를 다시 받아도 감사는 한 번이다
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
    // 목적지 열은 모두 있지만 종류가 해석되지 않는 값(스키마는 받는다)
    for id in ["a-odd", "c-both"] {
        db.sql(&format!(
            "update bot_user_schedules set dest_binding = 'slack', dest_kind = 'weird', dest_id = 'C9' where id = '{id}'"
        ))
        .await;
    }
    let before = fingerprint(&db).await;
    let page = users(&db).due(None, 10).await.unwrap();
    assert_eq!(ids(&page), ["b-ok"], "정상 행은 그대로 한 쪽에 담긴다");
    assert_eq!(
        page.unreadable
            .iter()
            .map(|u| (u.id.0.as_str(), u.reason, u.at))
            .collect::<Vec<_>>(),
        [
            ("a-odd", CloseReason::InvalidDestination, at(now - 50_000)),
            // 시각 정의도 목적지도 읽을 수 없으면 시각 정의가 먼저다
            ("c-both", CloseReason::InvalidTiming, at(now - 30_000)),
        ]
    );
    assert_eq!(
        fingerprint(&db).await,
        before,
        "읽기는 행을 건드리지 않는다"
    );
    // 정상 행의 `at`은 그 행의 다음 확인 시각이다
    assert_eq!(page.schedules[0].at, at(now - 40_000));
    // 그 사유로 닫으면 사유가 감사에 남고 원래 대화 안내는 없다
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
