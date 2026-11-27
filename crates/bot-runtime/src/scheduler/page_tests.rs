//! Keyset pagination tests for user schedule passes: held rows, fixed pass boundary, and error resume.

use super::user_tests::{FakeUsers, due, scheduler, snapshot, utc};
use bot_core::{
    id::{BindingId, RunId, ScheduleId},
    port::Error,
    schedule::{DeliveryTarget, DueSchedule, Timing},
    state::{Conversation, ConversationKind},
};
use std::time::Duration;

#[tokio::test]
async fn a_full_page_with_no_progress_moves_on_to_the_next_page_and_rests_at_the_end_of_the_pass() {
    let now = utc(2026, 11, 24, 10, 0);
    let one = |id: &str| {
        let mut s = due(Timing::At(utc(2026, 11, 24, 9, 0)), utc(2026, 11, 24, 8, 0));
        s.id = ScheduleId(id.into());
        s
    };
    let users = FakeUsers::new(now, vec![one("a"), one("b"), one("c")]);
    users
        .reply
        .lock()
        .unwrap()
        .extend([Err(Error::Stale), Err(Error::Stale)]);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2);
    assert!(
        s.tick_users().await.unwrap().backlog,
        "full page should advance to the next page"
    );
    let last = s.tick_users().await.unwrap();
    assert!(!last.backlog, "pass reached end and should rest");
    assert_eq!(
        users.advanced.lock().unwrap().len(),
        3,
        "first two entries must not be re-read before the third"
    );
    let asked = users.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2);
    assert!(
        asked[0].is_none()
            && asked[1]
                .as_ref()
                .is_some_and(|k| k.after.id.0 == "b" && k.end.id.0 == "c"),
        "{asked:?}"
    );
    s.tick_users().await.unwrap();
    assert!(users.asked.lock().unwrap()[2].is_none());
    let users = FakeUsers::new(now, vec![one("a")]);
    *users.due_error.lock().unwrap() = Some(Error::Stale);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2);
    assert!(matches!(s.tick_users().await, Err(Error::Stale)));
    let (_keep, shutdown) = tokio::sync::watch::channel(false);
    tokio::time::timeout(Duration::from_secs(2), s.run(shutdown))
        .await
        .expect("stale active set must stop the scheduler loop");
    assert!(users.advanced.lock().unwrap().is_empty());
}

fn held_and_free() -> (Vec<DueSchedule>, DueSchedule) {
    let target = DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: Conversation {
            kind: ConversationKind::Channel,
            id: "T1/C9".into(),
            thread: None,
        },
    };
    let held: Vec<DueSchedule> = (1..=5)
        .map(|i| {
            let mut s = due(
                Timing::At(utc(2026, 11, 24, 9, 0)),
                utc(2026, 11, 24, 8, 0) + Duration::from_secs(i),
            );
            s.id = ScheduleId(format!("held-{i}"));
            s.spec.destination = Some(target.clone());
            s
        })
        .collect();
    let mut free = due(
        Timing::At(utc(2026, 11, 24, 9, 0)),
        utc(2026, 11, 24, 8, 30),
    );
    free.id = ScheduleId("free".into());
    (held, free)
}

#[tokio::test]
async fn held_schedules_in_the_first_pages_do_not_block_the_one_behind_them_and_each_is_seen_once_per_pass()
 {
    let now = utc(2026, 11, 24, 10, 0);
    let (held, free) = held_and_free();
    let users = FakeUsers::new(now, held.into_iter().chain([free]).collect());
    *users.fires.lock().unwrap() = Some(RunId("run-free".into()));
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2);
    let (mut fired, mut ticks) = (0, 0);
    loop {
        let tick = s.tick_users().await.unwrap();
        (fired, ticks) = (fired + tick.fired, ticks + 1);
        assert!(ticks < 20, "pass did not terminate");
        if !tick.backlog {
            break;
        }
    }
    assert_eq!(fired, 1, "unblocked schedule must fire within one pass");
    {
        let advanced = users.advanced.lock().unwrap();
        assert_eq!(
            advanced.len(),
            1,
            "held schedules must neither fire nor close"
        );
        assert_eq!(advanced[0].id.0, "free");
    }
    let asked: Vec<Option<String>> = users
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|k| k.as_ref().map(|k| k.after.id.0.clone()))
        .collect();
    assert_eq!(
        asked,
        [None, Some("held-2".into()), Some("held-4".into())],
        "each page must resume after the previous page's last row"
    );
    s.tick_users().await.unwrap();
    assert!(users.asked.lock().unwrap()[3].is_none());
}

fn newcomer(n: usize) -> DueSchedule {
    let mut s = due(
        Timing::At(utc(2026, 11, 24, 9, 0)),
        utc(2026, 11, 24, 9, 0) + Duration::from_secs(n as u64),
    );
    s.id = ScheduleId(format!("n-{n}"));
    s
}

#[tokio::test]
async fn the_end_of_a_pass_is_fixed_when_it_starts_so_a_steady_inflow_cannot_stretch_it() {
    let now = utc(2026, 11, 24, 10, 0);
    let (held, free) = held_and_free();
    let users = FakeUsers::new(now, held.into_iter().chain([free]).collect());
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2);
    let mut ticks = 1;
    assert!(s.tick_users().await.unwrap().backlog);
    loop {
        users.due.lock().unwrap().push(newcomer(ticks));
        let tick = s.tick_users().await.unwrap();
        ticks += 1;
        assert!(ticks <= 6, "inflow must not prevent the pass from ending");
        if !tick.backlog {
            break;
        }
    }
    assert_eq!(
        ticks, 3,
        "pass should stop at the boundary fixed when it began"
    );
    let ends: Vec<String> = users
        .asked
        .lock()
        .unwrap()
        .iter()
        .flatten()
        .map(|k| k.end.id.0.clone())
        .collect();
    assert_eq!(ends, ["free", "free"], "pass end key must stay fixed");
    assert!(
        users
            .advanced
            .lock()
            .unwrap()
            .iter()
            .all(|a| !a.id.0.starts_with("n-")),
        "newcomers must wait for the next pass"
    );
    let mut more = 0;
    loop {
        let tick = s.tick_users().await.unwrap();
        more += 1;
        assert!(more <= 6);
        if !tick.backlog {
            break;
        }
    }
    let advanced: Vec<String> = users
        .advanced
        .lock()
        .unwrap()
        .iter()
        .map(|a| a.id.0.clone())
        .collect();
    assert!(
        advanced.contains(&"n-1".to_owned()) && advanced.contains(&"n-2".to_owned()),
        "{advanced:?}"
    );
}

#[tokio::test]
async fn a_failed_read_or_a_storage_error_keeps_the_resume_point_so_the_same_page_is_read_again() {
    let now = utc(2026, 11, 24, 10, 0);
    let (held, free) = held_and_free();
    let users = FakeUsers::new(now, held.into_iter().chain([free]).collect());
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2);
    assert!(s.tick_users().await.unwrap().backlog);
    *users.due_error.lock().unwrap() = Some(Error::Unavailable);
    assert!(matches!(s.tick_users().await, Err(Error::Unavailable)));
    *users.due_error.lock().unwrap() = None;
    assert!(s.tick_users().await.unwrap().backlog);
    let asked: Vec<Option<String>> = users
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|k| k.as_ref().map(|k| k.after.id.0.clone()))
        .collect();
    assert_eq!(
        asked,
        [None, Some("held-2".into()), Some("held-2".into())],
        "failed read must retry from the same cursor"
    );
    let mk = |id: &str, secs: u64| {
        let mut s = due(
            Timing::At(utc(2026, 11, 24, 9, 0)),
            utc(2026, 11, 24, 8, 0) + Duration::from_secs(secs),
        );
        s.id = ScheduleId(id.into());
        s
    };
    let users = FakeUsers::new(now, vec![mk("f1", 1), mk("f2", 2), mk("f3", 3)]);
    users.reply.lock().unwrap().push(Err(Error::Unavailable));
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2);
    assert!(matches!(s.tick_users().await, Err(Error::Unavailable)));
    assert!(s.tick_users().await.unwrap().backlog);
    assert!(!s.tick_users().await.unwrap().backlog);
    let asked: Vec<Option<String>> = users
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|k| k.as_ref().map(|k| k.after.id.0.clone()))
        .collect();
    assert_eq!(asked, [None, None, Some("f2".into())], "{asked:?}");
}
