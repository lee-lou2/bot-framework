//! Keyset pagination for due user schedules (PG): scans `(at, profile, id)` without gaps or
//! duplicates, freezes the sweep upper bound at start, and never mutates rows during reads.

use crate::support::{schedule::*, *};
use bot_core::{id::*, schedule::*, state::*};
use std::collections::BTreeSet;

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

async fn plant(db: &TestDb, origin: &RunContext, id: &str, now: i64, ago: i64) {
    db.sql(&format!(
        "insert into bot_user_schedules (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
                ceiling, at_ms, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{OWNER}', 'main', '{}', 'c-{id}', 'desk', 'direct', 'c1',
                 '{{run,calendar.read}}', {}, 'check', {}, {})",
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

fn key(after: (&str, i64), end: (&str, i64)) -> DueKey {
    DueKey {
        after: pos(after.0, after.1),
        end: pos(end.0, end.1),
    }
}

fn ids(page: &DuePage) -> Vec<String> {
    page.schedules.iter().map(|s| s.id.0.clone()).collect()
}

/// Inserts five due schedules where `c` and `d` tie on `next_ms` to test `(profile, id)` tie-breaking.
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
        "next key pairs last returned row with frozen sweep end"
    );
    let p2 = users(&db).due(p1.next, 2).await.unwrap();
    assert_eq!(ids(&p2), ["c", "d"]);
    assert_eq!(
        p2.next,
        Some(key(d, e)),
        "sweep end stays fixed across pages"
    );
    let p3 = users(&db).due(p2.next, 2).await.unwrap();
    assert_eq!(ids(&p3), ["e"]);
    assert_eq!(p3.next, None, "reaching end finishes the pass");

    let tie = users(&db).due(Some(key(c, e)), 1).await.unwrap();
    assert_eq!(ids(&tie), ["d"]);
    assert_eq!(tie.next, Some(key(d, e)));

    let full = users(&db).due(Some(key(b, e)), 3).await.unwrap();
    assert_eq!(ids(&full), ["c", "d", "e"]);
    assert!(full.next.is_none());

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
        assert!(round < 10, "keyset pagination did not terminate");
        let page = users(&db).due(after, 2).await.unwrap();
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(rows().await, before, "due scans must remain read-only");

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
        .expect("fired run");
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

    // An older row inserted mid-sweep appears on the next full pass rather than disrupting this one.
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
    plant(&db, &origin, "w", now, -3_600_000).await;
    let p1 = users(&db).due(None, 2).await.unwrap();
    assert_eq!(ids(&p1), ["a", "b"]);
    assert_eq!(
        p1.next.as_ref().map(|k| k.end.clone()),
        Some(pos(e.0, e.1)),
        "sweep end pins the last due row at sweep start"
    );
    db.sql("update bot_user_schedules set next_ms = bot_now_ms() - 1000 where id = 'w'")
        .await;

    let mut seen = ids(&p1);
    let mut next = p1.next;
    for round in 0.. {
        assert!(round < 10, "new arrivals prevented the pass from finishing");
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
        "arrivals after the sweep end wait for the next pass"
    );
    let again = ids(&users(&db).due(None, 50).await.unwrap());
    for id in ["a", "b", "c", "d", "e", "w", "n0"] {
        assert!(again.contains(&id.to_owned()), "{id}: {again:?}");
    }
}
