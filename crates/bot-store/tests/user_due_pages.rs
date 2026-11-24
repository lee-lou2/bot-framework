//! 확인할 예약을 이어 읽기(keyset, PG): `(다음 확인 시각, 프로필, ID)` 순서로 마지막으로 읽은 행 다음부터 — 빠지거나 겹치지 않고, 동률도 가르고,
//! 바퀴의 끝은 시작할 때 고정해(계속 새 예약이 들어와도 바퀴가 끝난다), 읽기는 행을 건드리지 않고, 읽는 사이 진전이 있어도 나머지를 가리지 않는다.

mod support;

use bot_core::{id::*, schedule::*, state::*};
use std::collections::BTreeSet;
use support::{schedule::*, *};

const OWNER: &str = "local:owner";

fn all(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn fire(at_ms: i64, next: Option<i64>, scopes: &[&str]) -> NextOccurrence {
    NextOccurrence::Fire {
        at: at(at_ms),
        next: next.map(at),
        scopes: all(scopes),
    }
}

fn advance(
    id: &ScheduleId,
    cursor: i64,
    revision: Option<&str>,
    next: NextOccurrence,
) -> AdvanceSchedule {
    AdvanceSchedule {
        profile: profile_id(),
        id: id.clone(),
        expected_cursor: at(cursor),
        checked_revision: revision.map(|r| Revision(r.into())),
        next,
    }
}

// ── 확인할 예약을 이어 읽기(keyset) ────────────────────────────────────────────────────────────

/// 예약 행을 SQL로 직접 넣는다: 정한 ID, 지금보다 `ago`ms 전이 «다음 확인 시각»인 한 번짜리. 같은 시각을 준 행은 (프로필, ID) 순서로 나온다.
async fn plant(db: &TestDb, origin: &RunContext, id: &str, now: i64, ago: i64) {
    db.sql(&format!(
        "insert into bot_user_schedules (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
                ceiling, at_ms, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{OWNER}', 'main', '{}', 'c-{id}', 'desk', 'direct', 'c1',
                 '{{run,calendar.read}}', {}, '확인', {}, {})",
        origin.attempt.run.0,
        now - ago,
        now - ago - 60_000,
        now - ago
    ))
    .await;
}

fn pos(id: &str, at_ms: i64) -> DuePos {
    DuePos {
        at: at(at_ms),
        profile: profile_id(),
        id: ScheduleId(id.into()),
    }
}

/// 이어 읽는 자리: `after`(직전 쪽의 마지막 행)와 바퀴의 끝 `end`.
fn key(after: (&str, i64), end: (&str, i64)) -> DueKey {
    DueKey {
        after: pos(after.0, after.1),
        end: pos(end.0, end.1),
    }
}

fn ids(page: &DuePage) -> Vec<String> {
    page.schedules.iter().map(|s| s.id.0.clone()).collect()
}

/// 원래 run이 이미 끝난 상태의 다섯 건: c · d는 다음 확인 시각이 같다(프로필 · ID 순서로 가른다). (지금, 출처 run)
async fn five(db: &TestDb) -> (i64, RunContext) {
    let origin = run_in(db, "r1", OWNER, ConversationKind::Direct).await;
    db.sql("update bot_runs set state = 'succeeded', lease_until_ms = null, lease_owner = null, finished_ms = bot_now_ms()")
        .await;
    let now = db.now_ms().await;
    for (id, ago) in [
        ("e", 10_000),
        ("c", 30_000),
        ("a", 50_000),
        ("d", 30_000),
        ("b", 40_000),
    ] {
        plant(db, &origin, id, now, ago).await;
    }
    (now, origin)
}

#[tokio::test]
async fn due_pages_continue_after_the_last_key_in_key_order_without_gaps_or_repeats() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 읽을 것이 없으면 빈 쪽이고 바퀴도 없다
    let none = users(&db).due(None, 10).await.unwrap();
    assert!(none.schedules.is_empty() && none.next.is_none());
    let (now, _) = five(&db).await;
    let (e, d, b, c) = (
        ("e", now - 10_000),
        ("d", now - 30_000),
        ("b", now - 40_000),
        ("c", now - 30_000),
    );
    let p1 = users(&db).due(None, 2).await.unwrap();
    assert_eq!(ids(&p1), ["a", "b"]);
    assert_eq!(
        p1.next,
        Some(key(b, e)),
        "직전 쪽의 마지막 행 + 바퀴를 시작할 때 고정한 끝(마지막 행 e)"
    );
    let p2 = users(&db).due(p1.next, 2).await.unwrap();
    assert_eq!(ids(&p2), ["c", "d"]);
    assert_eq!(p2.next, Some(key(d, e)), "끝은 쪽마다 같다");
    let p3 = users(&db).due(p2.next, 2).await.unwrap();
    assert_eq!(ids(&p3), ["e"]);
    assert_eq!(p3.next, None, "끝 행까지 읽었다 = 한 바퀴 끝");
    // 다음 확인 시각이 같은 둘(c · d) 사이에서 끊어도 빠지거나 겹치지 않는다
    let tie = users(&db).due(Some(key(c, e)), 1).await.unwrap();
    assert_eq!(ids(&tie), ["d"]);
    assert_eq!(tie.next, Some(key(d, e)));
    // 끝 행으로 딱 맞게 끝나는 쪽: 빈 쪽을 따로 읽지 않고 곧바로 한 바퀴 끝이다
    let full = users(&db).due(Some(key(b, e)), 3).await.unwrap();
    assert_eq!(ids(&full), ["c", "d", "e"]);
    assert!(full.next.is_none());
    // 처음부터: 한도에 딱 맞으면 한 바퀴 끝, 모자라면 이어 읽을 자리
    let all = users(&db).due(None, 5).await.unwrap();
    assert_eq!(ids(&all), ["a", "b", "c", "d", "e"]);
    assert!(all.next.is_none());
    let four = users(&db).due(None, 4).await.unwrap();
    assert_eq!(
        (ids(&four), four.next),
        (
            vec!["a".into(), "b".into(), "c".into(), "d".into()],
            Some(key(d, e))
        )
    );
    assert_eq!(
        users(&db).due(None, 100_000).await.unwrap().schedules.len(),
        5
    );
    // 끝 뒤의 자리에서는 읽을 것이 없다
    let past = users(&db).due(Some(key(("zz", now), e)), 10).await.unwrap();
    assert!(past.schedules.is_empty() && past.next.is_none());
}

#[tokio::test]
async fn reading_pages_never_touches_a_row_and_progress_between_pages_neither_repeats_nor_hides_the_rest()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (now, origin) = five(&db).await;
    let rows = || {
        db.scalar::<String>("select coalesce(md5(string_agg(id || ':' || cursor_ms || ':' || next_ms || ':' || state, ',' order by id)), '') from bot_user_schedules")
    };
    let before = rows().await;
    let mut after = None;
    for round in 0.. {
        assert!(round < 10, "이어 읽기가 끝나지 않는다");
        let page = users(&db).due(after, 2).await.unwrap();
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(
        rows().await,
        before,
        "읽기는 발생 시각 · 커서 · 다음 확인 시각 · 상태를 바꾸지 않는다"
    );
    // 첫 쪽(a · b)을 읽은 사이 a가 발화해 닫히고 b가 기다리게 됐다: 이어 읽기는 둘을 다시 읽지 않고 c · d · e를 그대로 준다
    let p1 = users(&db).due(None, 2).await.unwrap();
    assert_eq!(ids(&p1), ["a", "b"]);
    users(&db)
        .advance(advance(
            &ScheduleId("a".into()),
            now - 50_000 - 60_000,
            Some("r1"),
            fire(now - 50_000, None, &["run"]),
        ))
        .await
        .unwrap()
        .expect("발화");
    users(&db)
        .advance(advance(
            &ScheduleId("b".into()),
            now - 40_000 - 60_000,
            Some("r1"),
            NextOccurrence::WaitUntil(at(now + 3_600_000)),
        ))
        .await
        .unwrap();
    let p2 = users(&db).due(p1.next, 2).await.unwrap();
    assert_eq!(ids(&p2), ["c", "d"]);
    // 읽는 사이 더 오래된 시각으로 도래한 행은 이번 바퀴에는 없고 다음 바퀴(처음부터)에 맨 앞에 나온다
    plant(&db, &origin, "f", now, 90_000).await;
    assert_eq!(
        ids(&users(&db).due(p2.next.clone(), 10).await.unwrap()),
        ["e"]
    );
    assert_eq!(
        ids(&users(&db).due(None, 10).await.unwrap()),
        ["f", "c", "d", "e"]
    );
}

#[tokio::test]
async fn the_end_of_a_pass_is_fixed_at_its_start_so_new_arrivals_wait_for_the_next_pass() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (now, origin) = five(&db).await;
    let e = ("e", now - 10_000);
    // 아직 도래하지 않은 예약 하나(w)가 바퀴 도중에 도래한다
    plant(&db, &origin, "w", now, -3_600_000).await;
    let p1 = users(&db).due(None, 2).await.unwrap();
    assert_eq!(ids(&p1), ["a", "b"]);
    assert_eq!(
        p1.next.as_ref().map(|k| k.end.clone()),
        Some(pos(e.0, e.1)),
        "바퀴의 끝 = 시작할 때 확인할 예약의 마지막 행(e)"
    );
    db.sql("update bot_user_schedules set next_ms = bot_now_ms() - 1000 where id = 'w'")
        .await;
    // 바퀴가 이어지는 동안 점검마다 새 예약이 한 건씩 들어온다(쪽이 늘 가득 차는 유입)
    let mut seen = ids(&p1);
    let mut next = p1.next;
    for round in 0.. {
        assert!(round < 10, "유입 때문에 바퀴가 끝나지 않는다");
        plant(&db, &origin, &format!("n{round}"), db.now_ms().await, 500).await;
        let page = users(&db).due(next, 2).await.unwrap();
        seen.extend(ids(&page));
        next = page.next;
        if next.is_none() {
            break;
        }
    }
    assert_eq!(
        seen,
        ["a", "b", "c", "d", "e"],
        "끝 뒤에 들어오거나 도래한 예약(w · n0 …)은 이번 바퀴에서 빠진다"
    );
    // 다음 바퀴는 처음부터이고 그 예약들도 본다
    let again = ids(&users(&db).due(None, 50).await.unwrap());
    for id in ["a", "b", "c", "d", "e", "w", "n0"] {
        assert!(again.contains(&id.to_owned()), "{id}: {again:?}");
    }
}
