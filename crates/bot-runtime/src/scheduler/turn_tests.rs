//! 사용자 예약 한 바퀴의 «예약마다 한 번씩만 차례»와 «손상 행은 그 행만 닫기»의 스케줄러 쪽 시험(가짜 저장소).
//! 가짜 저장소는 `moves`를 켜면 실제 저장소처럼 행이 움직인다: 발화한 예약의 정렬 키가 앞으로 가서 같은 바퀴의 뒤쪽 쪽에 다시 나타난다.

use super::user_tests::{FakeUsers, cron, due, scheduler, snapshot, utc};
use bot_core::{
    id::{ProfileId, Revision, RunId, ScheduleId},
    port::Error,
    schedule::{CloseReason, DueSchedule, NextOccurrence, Timing, UnreadableSchedule},
};
use std::time::{Duration, SystemTime};

const NOW: fn() -> SystemTime = || utc(2026, 11, 24, 10, 0);

/// 30분 밀린 매분 반복 예약: 09:31 … 10:00의 서른 번이 밀렸다.
fn overdue_every_minute() -> DueSchedule {
    let mut s = due(cron("* * * * *", "UTC"), utc(2026, 11, 24, 9, 30));
    s.id = ScheduleId("r-every-minute".into());
    s
}

/// 한 번짜리 예약(정렬 키 = 커서). 반복 예약이 움직여도 그 뒤에 있도록 09:55 이후로 둔다.
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
    // 첫 바퀴: 반복 예약이 발화해 정렬 키가 앞으로 가도 같은 바퀴에서 두 번째 차례는 없다 — 뒤의 두 예약이 곧바로 이어서 발화한다
    for _ in 0..4 {
        s.tick_users().await.unwrap();
    }
    let first = fired(&users);
    assert_eq!(
        ids(&first),
        ["r-every-minute", "h-one-shot", "z-one-shot"],
        "예약마다 한 번씩"
    );
    assert_eq!(first[0].1, utc(2026, 11, 24, 9, 31), "가장 오래된 발생부터");
    // 바퀴를 마쳤으니 차례 기록을 비웠다: 다음 바퀴는 처음부터 읽고 남은 발생을 이어 간다
    s.tick_users().await.unwrap();
    assert!(
        users.asked.lock().unwrap().last().unwrap().is_none(),
        "다음 바퀴는 처음부터"
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
        "남은 발생을 오래된 순으로 이어 간다"
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
        assert!(ticks <= 40, "점검이 끝나지 않는다");
        if !tick.backlog {
            break;
        }
    }
    let fires = fired(&users);
    assert_eq!(fires.len(), 30, "밀린 서른 번을 쉬지 않고 이어서 접수했다");
    for (i, (_, at)) in fires.iter().enumerate() {
        assert_eq!(
            *at,
            utc(2026, 11, 24, 9, 31) + Duration::from_secs(60 * i as u64),
            "{i}번째"
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
    // 둘째 f2의 advance가 일시적으로 실패한다(첫째는 접수, 마지막 응답이 먼저 나간다)
    users
        .reply
        .lock()
        .unwrap()
        .extend([Err(Error::Unavailable), Ok(Some(RunId("run-f1".into())))]);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 3);
    assert!(matches!(s.tick_users().await, Err(Error::Unavailable)));
    let first: Vec<String> = fired(&users).into_iter().map(|(id, _)| id).collect();
    assert_eq!(first, ["f1", "f2"]);
    // 같은 쪽을 다시 읽는다: 처리를 마친 f1은 다시 하지 않고, 실패한 f2와 그 뒤 f3을 이어서 처리한다
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
        assert_eq!(
            bad.checked_revision,
            Some(Revision("r5".into())),
            "현재 판본으로 닫는다"
        );
        assert_eq!(bad.expected_cursor, utc(2026, 11, 24, 8, 0), "읽은 커서");
        assert!(
            advanced
                .iter()
                .any(|a| a.id.0 == "ok" && matches!(a.next, NextOccurrence::Fire { .. }))
        );
    }
    // 닫았으니 다음 바퀴에는 나오지 않는다
    assert_eq!(s.tick_users().await.unwrap().closed, 0);
    // 프로필이 활성이 아니면 판본 없이 비활성 종료로 닫는다(저장소가 실제 비활성인지 확인한다)
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
    // 손상 행의 닫기를 저장소가 받지 않는다(거절): 그 행만 건너뛰고 나머지는 처리한다. 응답은 뒤에서부터 나간다(손상 행이 먼저)
    users
        .reply
        .lock()
        .unwrap()
        .extend([Ok(Some(RunId("run-ok".into()))), Err(Error::Denied)]);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    let tick = s.tick_users().await.unwrap();
    assert_eq!((tick.fired, tick.closed), (1, 0));
    // 저장소가 응답하지 못하면 오류로 올린다(손상으로 다루지도, 삼키지도 않는다) — 다음 점검이 같은 쪽에서 다시 시도한다
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
        "손상 행이 막히면 뒤 행도 이번 점검에서는 하지 않는다"
    );
    let tick = s.tick_users().await.unwrap();
    assert_eq!((tick.closed, tick.fired), (1, 0), "{tick:?}");
}

/// 정상 한 번짜리와 손상 행의 처리 순서(저장소를 흉내 낸 `advance` 기록의 예약 ID 순서).
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
    // 손상 행이 정상 행보다 뒤에 온다: 정상 행이 먼저다(손상 행의 잠금을 기다리느라 앞의 정상 행이 밀리지 않는다)
    let users = FakeUsers::new(NOW(), vec![one_shot("a-ok", 55), one_shot("c-ok", 57)]);
    *users.fires.lock().unwrap() = Some(RunId("run".into()));
    users
        .unreadable
        .lock()
        .unwrap()
        .push(unreadable("b-bad", utc(2026, 11, 24, 9, 56)));
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    s.tick_users().await.unwrap();
    assert_eq!(
        handled(&users),
        ["a-ok", "b-bad", "c-ok"],
        "저장소가 읽은 순서"
    );
    // 손상 행이 가장 앞이면 그 행이 먼저다
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
    // 같은 시각이면 (프로필, ID) 순서다
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
    // 30분 밀린 매분 반복 예약 + 마지막에 보류될 목적지 예약(등록부가 없어 발화도 닫기도 하지 않는다). 배치 1 → 바퀴 하나가 점검 세 번이다
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
        assert!(ticks <= 200, "점검이 끝나지 않는다");
        if !tick.backlog {
            break;
        }
    }
    // 첫 바퀴(점검 1~3): 반복 예약이 발화한 «남음»이 쪽을 넘어도 바퀴가 끝나는 3번째 점검에서 전해진다(쉬지 않는다)
    assert_eq!(&backlogs[..3], [true, true, true], "{backlogs:?}");
    // 밀린 서른 번을 모두 따라잡은 다음에야 쉰다: 보류 예약만 남았고, 쉬기 직전 밀린 정상 발생은 없다
    let fires = fired(&users);
    let recurring: Vec<_> = fires
        .iter()
        .filter(|(id, _)| id == "r-every-minute")
        .collect();
    assert_eq!(recurring.len(), 30, "{}", fires.len());
    assert_eq!(ids(&fires).iter().filter(|id| **id == "h-ok").count(), 1);
    assert!(
        !ids(&fires).contains(&"z-held"),
        "보류 예약은 발화하지 않았다"
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
        "쉬기 직전 밀린 정상 발생은 0건이다"
    );
    assert!(!backlogs[backlogs.len() - 1] && backlogs[..backlogs.len() - 1].iter().all(|b| *b));
}
