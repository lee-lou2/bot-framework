//! Per-pass turn fairness and isolated unreadable-row closing tests for user schedules.

use super::user_tests::{FakeUsers, cron, due, scheduler, snapshot, utc};
use bot_core::{
    id::{ProfileId, Revision, RunId, ScheduleId},
    port::Error,
    schedule::{CloseReason, DueSchedule, NextOccurrence, Timing, UnreadableSchedule},
};
use std::time::{Duration, SystemTime};

const NOW: fn() -> SystemTime = || utc(2026, 11, 24, 10, 0);

fn overdue_every_minute() -> DueSchedule {
    let mut s = due(cron("* * * * *", "UTC"), utc(2026, 11, 24, 9, 30));
    s.id = ScheduleId("r-every-minute".into());
    s
}

fn one_shot(id: &str, cursor_minute: u32) -> DueSchedule {
    let mut s = due(
        Timing::At(utc(2026, 11, 24, 9, 58)),
        utc(2026, 11, 24, 9, cursor_minute),
    );
    s.id = ScheduleId(id.into());
    s
}

fn unreadable(id: &str, cursor: SystemTime) -> UnreadableSchedule {
    UnreadableSchedule {
        profile: ProfileId("personal".into()),
        id: ScheduleId(id.into()),
        cursor,
        at: cursor,
        reason: CloseReason::InvalidTiming,
    }
}

fn fired(users: &FakeUsers) -> Vec<(String, SystemTime)> {
    users
        .advanced
        .lock()
        .unwrap()
        .iter()
        .filter_map(|a| match &a.next {
            NextOccurrence::Fire { at, .. } => Some((a.id.0.clone(), *at)),
            _ => None,
        })
        .collect()
}

fn ids(fires: &[(String, SystemTime)]) -> Vec<&str> {
    fires.iter().map(|(id, _)| id.as_str()).collect()
}

#[tokio::test]
async fn a_schedule_that_moves_forward_when_it_fires_gets_one_turn_per_pass_so_the_others_are_not_starved()
 {
    let users = FakeUsers::new(
        NOW(),
        vec![
            overdue_every_minute(),
            one_shot("h-one-shot", 55),
            one_shot("z-one-shot", 56),
        ],
    );
    *users.fires.lock().unwrap() = Some(RunId("run".into()));
    *users.moves.lock().unwrap() = true;
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 1);
    for _ in 0..4 {
        s.tick_users().await.unwrap();
    }
    let first = fired(&users);
    assert_eq!(
        ids(&first),
        ["r-every-minute", "h-one-shot", "z-one-shot"],
        "one turn per schedule in the first pass"
    );
    assert_eq!(first[0].1, utc(2026, 11, 24, 9, 31), "oldest due first");
    s.tick_users().await.unwrap();
    assert!(
        users.asked.lock().unwrap().last().unwrap().is_none(),
        "next pass restarts from the beginning"
    );
    let all = fired(&users);
    assert_eq!(
        ids(&all),
        [
            "r-every-minute",
            "h-one-shot",
            "z-one-shot",
            "r-every-minute"
        ]
    );
    assert_eq!(
        all[3].1,
        utc(2026, 11, 24, 9, 32),
        "continues remaining occurrences oldest first"
    );
}

#[tokio::test]
async fn a_fired_schedule_with_more_due_occurrences_asks_for_the_next_pass_right_away_oldest_first()
{
    let users = FakeUsers::new(NOW(), vec![overdue_every_minute()]);
    *users.fires.lock().unwrap() = Some(RunId("run".into()));
    *users.moves.lock().unwrap() = true;
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 100);
    let mut ticks = 0;
    loop {
        let tick = s.tick_users().await.unwrap();
        ticks += 1;
        assert!(ticks <= 40, "ticks did not converge");
        if !tick.backlog {
            break;
        }
    }
    let fires = fired(&users);
    assert_eq!(fires.len(), 30, "caught up all 30 overdue occurrences");
    for (i, (_, at)) in fires.iter().enumerate() {
        assert_eq!(
            *at,
            utc(2026, 11, 24, 9, 31) + Duration::from_secs(60 * i as u64),
            "occurrence #{i}"
        );
    }
}

#[tokio::test]
async fn a_storage_error_midway_retries_the_row_that_failed_and_the_ones_behind_it_but_not_the_ones_already_served()
 {
    let users = FakeUsers::new(
        NOW(),
        vec![one_shot("f1", 55), one_shot("f2", 56), one_shot("f3", 57)],
    );
    // Replies pop from the back: f1 succeeds, then f2 fails transiently.
    users
        .reply
        .lock()
        .unwrap()
        .extend([Err(Error::Unavailable), Ok(Some(RunId("run-f1".into())))]);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 3);
    assert!(matches!(s.tick_users().await, Err(Error::Unavailable)));
    let first: Vec<String> = fired(&users).into_iter().map(|(id, _)| id).collect();
    assert_eq!(first, ["f1", "f2"]);
    s.tick_users().await.unwrap();
    let all: Vec<String> = fired(&users).into_iter().map(|(id, _)| id).collect();
    assert_eq!(all, ["f1", "f2", "f2", "f3"]);
}

#[tokio::test]
async fn an_unreadable_row_is_closed_on_its_own_and_the_rest_of_the_page_is_handled() {
    let users = FakeUsers::new(NOW(), vec![one_shot("ok", 55)]);
    users
        .unreadable
        .lock()
        .unwrap()
        .push(unreadable("bad", utc(2026, 11, 24, 8, 0)));
    *users.fires.lock().unwrap() = Some(RunId("run-ok".into()));
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    let tick = s.tick_users().await.unwrap();
    assert_eq!((tick.fired, tick.closed), (1, 1));
    {
        let advanced = users.advanced.lock().unwrap();
        let bad = advanced.iter().find(|a| a.id.0 == "bad").unwrap();
        assert!(matches!(
            bad.next,
            NextOccurrence::Close(CloseReason::InvalidTiming)
        ));
        assert_eq!(bad.checked_revision, Some(Revision("r5".into())));
        assert_eq!(bad.expected_cursor, utc(2026, 11, 24, 8, 0));
        assert!(
            advanced
                .iter()
                .any(|a| a.id.0 == "ok" && matches!(a.next, NextOccurrence::Fire { .. }))
        );
    }
    assert_eq!(s.tick_users().await.unwrap().closed, 0);
    let users = FakeUsers::new(NOW(), vec![]);
    users
        .unreadable
        .lock()
        .unwrap()
        .push(unreadable("bad", utc(2026, 11, 24, 8, 0)));
    let s = scheduler(&users, None, 10);
    assert_eq!(s.tick_users().await.unwrap().closed, 1);
    let advanced = users.advanced.lock().unwrap();
    assert!(advanced[0].checked_revision.is_none());
    assert!(matches!(
        advanced[0].next,
        NextOccurrence::Close(CloseReason::ProfileInactive)
    ));
}

#[tokio::test]
async fn a_refused_close_of_an_unreadable_row_does_not_stop_the_page_but_an_unavailable_store_does()
{
    let users = FakeUsers::new(NOW(), vec![one_shot("ok", 55)]);
    users
        .unreadable
        .lock()
        .unwrap()
        .push(unreadable("bad", utc(2026, 11, 24, 8, 0)));
    users
        .reply
        .lock()
        .unwrap()
        .extend([Ok(Some(RunId("run-ok".into()))), Err(Error::Denied)]);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    let tick = s.tick_users().await.unwrap();
    assert_eq!((tick.fired, tick.closed), (1, 0));
    let users = FakeUsers::new(NOW(), vec![one_shot("ok", 55)]);
    users
        .unreadable
        .lock()
        .unwrap()
        .push(unreadable("bad", utc(2026, 11, 24, 8, 0)));
    users.reply.lock().unwrap().push(Err(Error::Unavailable));
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    assert!(matches!(s.tick_users().await, Err(Error::Unavailable)));
    assert!(
        fired(&users).is_empty(),
        "unavailable error on unreadable row must abort the page"
    );
    let tick = s.tick_users().await.unwrap();
    assert_eq!((tick.closed, tick.fired), (1, 0), "{tick:?}");
}

fn handled(users: &FakeUsers) -> Vec<String> {
    users
        .advanced
        .lock()
        .unwrap()
        .iter()
        .map(|a| a.id.0.clone())
        .collect()
}

#[tokio::test]
async fn a_page_with_healthy_and_unreadable_rows_is_handled_in_the_stores_order() {
    let users = FakeUsers::new(NOW(), vec![one_shot("a-ok", 55), one_shot("c-ok", 57)]);
    *users.fires.lock().unwrap() = Some(RunId("run".into()));
    users
        .unreadable
        .lock()
        .unwrap()
        .push(unreadable("b-bad", utc(2026, 11, 24, 9, 56)));
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    s.tick_users().await.unwrap();
    assert_eq!(handled(&users), ["a-ok", "b-bad", "c-ok"]);
    let users = FakeUsers::new(NOW(), vec![one_shot("b-ok", 56)]);
    *users.fires.lock().unwrap() = Some(RunId("run".into()));
    users
        .unreadable
        .lock()
        .unwrap()
        .push(unreadable("z-bad", utc(2026, 11, 24, 9, 40)));
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    s.tick_users().await.unwrap();
    assert_eq!(handled(&users), ["z-bad", "b-ok"]);
    let users = FakeUsers::new(NOW(), vec![one_shot("m-ok", 56)]);
    users
        .unreadable
        .lock()
        .unwrap()
        .push(unreadable("a-bad", utc(2026, 11, 24, 9, 56)));
    users
        .unreadable
        .lock()
        .unwrap()
        .push(unreadable("z-bad", utc(2026, 11, 24, 9, 56)));
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    s.tick_users().await.unwrap();
    assert_eq!(handled(&users), ["a-bad", "m-ok", "z-bad"]);
}

#[tokio::test]
async fn an_unreadable_row_is_closed_with_the_reason_the_store_gave() {
    for reason in [CloseReason::InvalidTiming, CloseReason::InvalidDestination] {
        let users = FakeUsers::new(NOW(), vec![]);
        let mut bad = unreadable("bad", utc(2026, 11, 24, 9, 0));
        bad.reason = reason;
        users.unreadable.lock().unwrap().push(bad);
        let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
        s.tick_users().await.unwrap();
        let advanced = users.advanced.lock().unwrap();
        assert!(
            matches!(advanced[0].next, NextOccurrence::Close(got) if got == reason),
            "{reason:?}"
        );
    }
}

#[tokio::test]
async fn the_remaining_due_occurrences_survive_the_page_boundary_and_are_told_when_the_pass_ends() {
    let mut held = one_shot("z-held", 58);
    held.spec.destination = Some(bot_core::schedule::DeliveryTarget {
        binding: bot_core::id::BindingId("slack".into()),
        conversation: bot_core::state::Conversation {
            kind: bot_core::state::ConversationKind::Channel,
            id: "T1/C9".into(),
            thread: None,
        },
    });
    let users = FakeUsers::new(
        NOW(),
        vec![overdue_every_minute(), one_shot("h-ok", 55), held],
    );
    *users.fires.lock().unwrap() = Some(RunId("run".into()));
    *users.moves.lock().unwrap() = true;
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 1);
    let (mut backlogs, mut ticks) = (Vec::new(), 0);
    loop {
        let tick = s.tick_users().await.unwrap();
        ticks += 1;
        backlogs.push(tick.backlog);
        assert!(ticks <= 200, "ticks did not converge");
        if !tick.backlog {
            break;
        }
    }
    assert_eq!(&backlogs[..3], [true, true, true], "{backlogs:?}");
    let fires = fired(&users);
    let recurring: Vec<_> = fires
        .iter()
        .filter(|(id, _)| id == "r-every-minute")
        .collect();
    assert_eq!(recurring.len(), 30, "{}", fires.len());
    assert_eq!(ids(&fires).iter().filter(|id| **id == "h-ok").count(), 1);
    assert!(
        !ids(&fires).contains(&"z-held"),
        "held schedule must not fire"
    );
    assert_eq!(
        users
            .due
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.id.0.clone())
            .collect::<Vec<_>>(),
        ["z-held"],
        "no overdue healthy schedule should remain before resting"
    );
    assert!(!backlogs[backlogs.len() - 1] && backlogs[..backlogs.len() - 1].iter().all(|b| *b));
}
