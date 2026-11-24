//! 사용자 예약을 이어 읽기(keyset)의 스케줄러 쪽 시험(가짜 저장소): 보류된 예약이 앞쪽을 채워도 뒤의 예약이 막히지 않고,
//! 쪽마다 한 번씩만 읽으며, 저장소 오류로 빠져나와도 이어 읽을 자리가 그대로이고, 한 바퀴를 마치면 처음부터 다시 본다.

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
    // 세 건 중 앞의 둘이 한 쪽을 채웠지만 둘 다 낡아(Stale) 진전이 없다: 같은 묶음을 다시 읽지 않고 그 뒤 쪽을 읽으러 가며(backlog),
    // 거기서 이 바퀴의 끝까지 읽었으면 한 바퀴를 마친 것이라 기본 간격으로 쉰다(backlog=false)
    let users = FakeUsers::new(now, vec![one("a"), one("b"), one("c")]);
    users
        .reply
        .lock()
        .unwrap()
        .extend([Err(Error::Stale), Err(Error::Stale)]);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2);
    assert!(
        s.tick_users().await.unwrap().backlog,
        "가득 찬 쪽의 다음 쪽을 읽으러 간다"
    );
    let last = s.tick_users().await.unwrap();
    assert!(!last.backlog, "이 바퀴의 끝까지 읽었다: 한 바퀴를 마쳤다");
    assert_eq!(
        users.advanced.lock().unwrap().len(),
        3,
        "앞의 두 건을 다시 읽어 보내지 않았다(셋째가 한 번)"
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
    // 한 바퀴를 마쳤으니 다음 점검은 처음부터 다시 본다
    s.tick_users().await.unwrap();
    assert!(users.asked.lock().unwrap()[2].is_none());
    // 옛 활성 집합에 묶인 스케줄러는 `due`에서 멈추고 루프를 끝낸다(스스로 계속 돌지 않는다)
    let users = FakeUsers::new(now, vec![one("a")]);
    *users.due_error.lock().unwrap() = Some(Error::Stale);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2);
    assert!(matches!(s.tick_users().await, Err(Error::Stale)));
    let (_keep, shutdown) = tokio::sync::watch::channel(false);
    tokio::time::timeout(Duration::from_secs(2), s.run(shutdown))
        .await
        .expect("교체된 집합이면 루프가 끝난다");
    assert!(users.advanced.lock().unwrap().is_empty());
}

/// 보류될 예약(명시 목적지가 있는데 등록부가 없다 → 발화도 닫기도 하지 않는다)과 막히지 않는 예약.
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
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2); // 등록부가 없어 목적지 예약은 보류된다
    let (mut fired, mut ticks) = (0, 0);
    loop {
        let tick = s.tick_users().await.unwrap();
        (fired, ticks) = (fired + tick.fired, ticks + 1);
        assert!(ticks < 20, "점검 주기가 끝나지 않는다");
        if !tick.backlog {
            break;
        }
    }
    assert_eq!(fired, 1, "막히지 않는 예약이 한 주기 안에 발화한다");
    {
        let advanced = users.advanced.lock().unwrap();
        assert_eq!(advanced.len(), 1, "보류 예약은 보내지도 닫지도 않았다");
        assert_eq!(advanced[0].id.0, "free");
    }
    // 여섯 건 = 둘씩 세 쪽: 쪽마다 한 번씩만 읽었고, 끝 행(free)을 읽은 쪽이 마지막이다(빈 쪽을 따로 읽지 않는다)
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
        "마지막으로 읽은 행 다음부터 이어 읽는다"
    );
    // 다음 주기는 처음부터
    s.tick_users().await.unwrap();
    assert!(users.asked.lock().unwrap()[3].is_none());
}

/// 끝 뒤에 놓이는 새 예약(커서가 더 늦다).
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
    // 바퀴를 시작한 뒤로 점검마다 새 예약이 한 건씩 들어온다(쪽이 늘 가득 차 바퀴가 끝나지 않는 상황)
    let mut ticks = 1;
    assert!(s.tick_users().await.unwrap().backlog);
    loop {
        users.due.lock().unwrap().push(newcomer(ticks));
        let tick = s.tick_users().await.unwrap();
        ticks += 1;
        assert!(ticks <= 6, "유입 때문에 바퀴가 끝나지 않는다");
        if !tick.backlog {
            break;
        }
    }
    assert_eq!(ticks, 3, "바퀴를 시작할 때 고정한 끝(free)까지 읽고 끝난다");
    let ends: Vec<String> = users
        .asked
        .lock()
        .unwrap()
        .iter()
        .flatten()
        .map(|k| k.end.id.0.clone())
        .collect();
    assert_eq!(ends, ["free", "free"], "이어 읽는 쪽마다 같은 끝");
    assert!(
        users
            .advanced
            .lock()
            .unwrap()
            .iter()
            .all(|a| !a.id.0.starts_with("n-")),
        "새 예약은 이번 바퀴에서 빠졌다"
    );
    // 다음 바퀴는 처음부터이고 새 예약도 본다
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
    assert!(s.tick_users().await.unwrap().backlog); // 첫 쪽(held-1·2)을 읽었다 → 다음은 held-2 뒤
    // 저장소가 읽기에 실패한다: 이어 읽을 자리를 바꾸지 않는다
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
        "실패한 읽기 뒤에도 같은 자리에서 다시 읽는다"
    );
    // 쪽을 처리하다 저장소가 응답하지 못해 빠져나와도 이어 읽을 자리는 그대로다: 다음 점검이 같은 쪽을 다시 읽는다(그 쪽을 건너뛰지 않는다)
    let mk = |id: &str, secs: u64| {
        let mut s = due(
            Timing::At(utc(2026, 11, 24, 9, 0)),
            utc(2026, 11, 24, 8, 0) + Duration::from_secs(secs),
        );
        s.id = ScheduleId(id.into());
        s
    };
    let users = FakeUsers::new(now, vec![mk("f1", 1), mk("f2", 2), mk("f3", 3)]);
    users.reply.lock().unwrap().push(Err(Error::Unavailable)); // f1의 advance가 실패한다
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2);
    assert!(matches!(s.tick_users().await, Err(Error::Unavailable)));
    assert!(s.tick_users().await.unwrap().backlog); // 같은 첫 쪽을 다시 읽고 끝까지 처리한다
    assert!(!s.tick_users().await.unwrap().backlog); // 그다음 쪽(f3) — 덜 찼으니 한 바퀴 끝
    let asked: Vec<Option<String>> = users
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|k| k.as_ref().map(|k| k.after.id.0.clone()))
        .collect();
    assert_eq!(asked, [None, None, Some("f2".into())], "{asked:?}");
}
