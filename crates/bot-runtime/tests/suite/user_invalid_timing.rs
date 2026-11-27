//! 저장된 시각 정의를 읽지 못한 사용자 예약(실제 PG 저장소 + 실제 스케줄러): 그 행만 `invalid_timing`으로 닫고, 같은 쪽의 정상 예약은 그대로 발화한다.
//! - 기한이 지난 두 행(손상 한 건 + 정상 일회성 한 건)을 같은 쪽에 두고 스케줄러를 돌린다: 손상 행만 closed/invalid_timing(그 행의 run · 발생 0, `schedule.close` 감사 1, 원래 대화 안내 0),
//!   정상 행은 run · 발생 1과 closed/finished. 다음 점검에서도 감사 · 안내 · 발생 수가 늘지 않는다.
//! - 대조군: 읽을 수 있는 cron + 해석할 수 없는 시간대는 같은 행별 결과다(저장소는 읽고, 스케줄러가 판단해 닫는다).
//! - 단독 `due()` 조회는 읽기 전용이다. 저장된 값이 읽히지 않는 것만 손상으로 다룬다.
//!
//! 사용자 예약 행은 SQL로 넣는다(저장소의 만들기 검사를 거치지 않는 손상 값을 만들기 위해서다). 모델 호출은 없다.

use crate::support::*;
use bot_broker::testing::{FakeCatalog, snapshot};
use bot_core::schedule::UserSchedules;
use bot_runtime::{Scheduler, SchedulerConfig, SchedulerDeps};
use std::sync::Arc;

fn scheduler(w: &World) -> Scheduler {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &["run", "calendar.read"])
            .member(OWNER, &["owner"])
            .policy_scopes(&["run", "calendar.read"])
            .slack_dm_binding("desk", "main")
            .build(),
    );
    Scheduler::new(
        SchedulerDeps {
            schedule: w.pg.store.clone(),
            user: w.pg.store.clone(),
            catalog,
            tracer: Arc::new(bot_core::trace::NoopTracer),
        },
        SchedulerConfig {
            interval: std::time::Duration::from_millis(10),
            batch: 100,
        },
    )
}

/// 기한이 지난 반복 예약 한 행: 저장된 `cron` · `timezone`을 그대로 넣는다(검사를 거치지 않는다).
async fn plant_cron(w: &World, id: &str, cron: &str, zone: &str, ago: i64) {
    let origin = w
        .accept_slack_dm(&format!("origin-{id}"), "예약을 만든 대화")
        .await;
    w.pg.sql(&format!(
        "insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, cron, timezone, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{OWNER}', 'main', '{}', 'c-{id}', 'desk', 'direct', 'owner',
            '{{run,calendar.read}}', '{cron}', '{zone}', '주간 보고를 보낸다', bot_now_ms() - 3600000, bot_now_ms() - {ago})",
        origin.0
    ))
    .await;
}

/// 기한이 지난 한 번짜리 한 행(정상).
async fn plant_once(w: &World, id: &str, ago: i64) {
    let origin = w
        .accept_slack_dm(&format!("origin-{id}"), "예약을 만든 대화")
        .await;
    w.pg.sql(&format!(
        "insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, at_ms, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{OWNER}', 'main', '{}', 'c-{id}', 'desk', 'direct', 'owner',
            '{{run,calendar.read}}', bot_now_ms() - {ago}, '확인', bot_now_ms() - 3600000, bot_now_ms() - {ago})",
        origin.0
    ))
    .await;
}

async fn count(w: &World, sql: &str) -> i64 {
    w.pg.scalar::<i64>(sql).await
}

/// 점검 사이에 늘면 안 되는 것들: run · 발생 · 감사 · 원래 대화 안내(outbox).
async fn totals(w: &World) -> [i64; 5] {
    [
        count(w, "select count(*) from bot_runs").await,
        count(w, "select count(*) from bot_user_occurrences").await,
        count(
            w,
            "select count(*) from bot_audit where action = 'schedule.close'",
        )
        .await,
        count(
            w,
            "select count(*) from bot_audit where action = 'schedule.fire'",
        )
        .await,
        count(w, "select count(*) from bot_outbox").await,
    ]
}

async fn state(w: &World, id: &str) -> String {
    w.pg.scalar::<String>(&format!(
        "select state || ':' || coalesce(close_reason, '') from bot_user_schedules where id = '{id}'"
    ))
    .await
}

#[tokio::test]
async fn a_row_whose_stored_timing_cannot_be_read_is_closed_alone_and_the_healthy_row_beside_it_fires()
 {
    // (설명, 저장된 cron, 저장된 시간대, 읽기 단계에서 «읽을 수 없는 행»으로 오는가)
    for (why, cron, zone, unreadable) in [
        ("저장된 cron을 읽을 수 없다", "not-a-cron", "UTC", true),
        (
            "대조군: 읽을 수 있는 cron + 해석할 수 없는 시간대",
            "0 9 * * *",
            "Invalid/Zone",
            false,
        ),
    ] {
        let Some(w) = world().await else {
            return;
        };
        plant_cron(&w, "u-bad", cron, zone, 120_000).await;
        plant_once(&w, "u-good", 90_000).await;
        let before = totals(&w).await;
        // 단독 조회는 읽기 전용이다: 같은 쪽에 두 행이 모두 있고, 몇 번을 읽어도 아무것도 바뀌지 않는다
        for _ in 0..2 {
            let page = w.pg.store.due(None, 100).await.unwrap();
            let healthy: Vec<_> = page.schedules.iter().map(|s| s.id.0.as_str()).collect();
            let broken: Vec<_> = page.unreadable.iter().map(|u| u.id.0.as_str()).collect();
            if unreadable {
                assert_eq!((healthy, broken), (vec!["u-good"], vec!["u-bad"]), "{why}");
            } else {
                assert_eq!(
                    (healthy, broken),
                    (vec!["u-bad", "u-good"], vec![]),
                    "{why}"
                );
            }
        }
        assert_eq!(totals(&w).await, before, "{why}");
        assert_eq!(
            count(
                &w,
                "select count(*) from bot_user_schedules where state = 'active'"
            )
            .await,
            2,
            "{why}"
        );

        let scheduler = scheduler(&w);
        let tick = scheduler.tick_users().await.unwrap();
        assert_eq!((tick.fired, tick.closed), (1, 1), "{why}");
        // 손상 행만 닫혔다: 발생 · run 0, 감사 한 줄(고정 낱말), 원래 대화 안내 0
        assert_eq!(state(&w, "u-bad").await, "closed:invalid_timing", "{why}");
        assert_eq!(
            count(
                &w,
                "select count(*) from bot_user_occurrences where schedule = 'u-bad'"
            )
            .await,
            0,
            "{why}"
        );
        assert_eq!(
            count(
                &w,
                "select count(*) from bot_audit where action = 'schedule.close' and target = 'u-bad' and verdict = 'invalid_timing'"
            )
            .await,
            1,
            "{why}"
        );
        // 정상 행은 발화하고 마지막 발생이라 정상 종료로 닫혔다
        assert_eq!(state(&w, "u-good").await, "closed:finished", "{why}");
        assert_eq!(
            count(
                &w,
                "select count(*) from bot_user_occurrences where schedule = 'u-good'"
            )
            .await,
            1,
            "{why}"
        );
        assert_eq!(
            count(
                &w,
                "select count(*) from bot_audit where action = 'schedule.close' and target = 'u-good' and verdict = 'finished'"
            )
            .await,
            1,
            "{why}"
        );
        let after = totals(&w).await;
        assert_eq!(
            [
                after[0] - before[0],
                after[1] - before[1],
                after[2] - before[2],
                after[3] - before[3],
                after[4] - before[4]
            ],
            [1, 1, 2, 1, 0],
            "{why}: 새 run 1(정상 행), 발생 1, 종료 감사 2, 발화 감사 1, 안내 0"
        );

        // 다음 점검에서도 감사 · 안내 · 발생이 늘지 않는다
        for _ in 0..2 {
            let again = scheduler.tick_users().await.unwrap();
            assert_eq!(
                (again.fired, again.closed, again.waiting),
                (0, 0, 0),
                "{why}"
            );
            assert_eq!(totals(&w).await, after, "{why}");
        }
    }
}

#[tokio::test]
async fn an_unreadable_row_ahead_of_many_healthy_ones_never_stalls_the_scheduler() {
    let Some(w) = world().await else {
        return;
    };
    plant_cron(&w, "a-bad", "not-a-cron", "UTC", 120_000).await;
    for n in 0..5 {
        plant_once(&w, &format!("ok-{n}"), 90_000).await;
    }
    // 배치 2: 손상 행이 첫 쪽에 섞여도 쪽은 앞으로 가고, 모든 정상 행이 한 바퀴 안에서 발화한다
    let scheduler = {
        let catalog = Arc::new(FakeCatalog::new());
        catalog.publish(
            snapshot("personal", "r1")
                .role("owner", &["run", "calendar.read"])
                .member(OWNER, &["owner"])
                .policy_scopes(&["run", "calendar.read"])
                .slack_dm_binding("desk", "main")
                .build(),
        );
        Scheduler::new(
            SchedulerDeps {
                schedule: w.pg.store.clone(),
                user: w.pg.store.clone(),
                catalog,
                tracer: Arc::new(bot_core::trace::NoopTracer),
            },
            SchedulerConfig {
                interval: std::time::Duration::from_millis(10),
                batch: 2,
            },
        )
    };
    let (mut fired, mut closed) = (0, 0);
    for round in 0.. {
        assert!(round < 20, "한 바퀴가 끝나지 않는다");
        let tick = scheduler.tick_users().await.unwrap();
        (fired, closed) = (fired + tick.fired, closed + tick.closed);
        if !tick.backlog {
            break;
        }
    }
    assert_eq!(
        (fired, closed),
        (5, 1),
        "정상 행 다섯은 발화(마지막 발생이라 같은 커밋에서 정상 종료), 손상 행 하나는 시각 오류로 닫힘"
    );
    assert_eq!(state(&w, "a-bad").await, "closed:invalid_timing");
    assert_eq!(
        count(
            &w,
            "select count(*) from bot_user_schedules where state = 'active'"
        )
        .await,
        0
    );
}

/// 조건이 될 때까지 기다린다(최대 10초).
async fn eventually(what: &str, mut ready: impl AsyncFnMut() -> bool) {
    for _ in 0..200 {
        if ready().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{what}");
}

#[tokio::test]
async fn a_healthy_row_ahead_of_a_locked_unreadable_row_fires_without_waiting_for_the_lock() {
    let Some(w) = world().await else {
        return;
    };
    // 정상 행이 먼저 도래하고 손상 행은 그보다 늦게 도래했다. 다른 연결이 손상 행을 잠그고 있다(다른 스케줄러가 같은 행을 닫는 중인 상황)
    plant_once(&w, "u-good", 120_000).await;
    plant_cron(&w, "u-bad", "not-a-cron", "UTC", 60_000).await;
    let mut lock = w.pg.store.pool().begin().await.unwrap();
    sqlx::query("select 1 from bot_user_schedules where id = 'u-bad' for update")
        .execute(&mut *lock)
        .await
        .unwrap();
    let scheduler = scheduler(&w);
    let tick = tokio::spawn(async move { scheduler.tick_users().await });
    // 스케줄러가 그 잠금을 기다리는 동안에도 앞의 정상 행은 발화한다
    eventually(
        "정상 행이 잠금 뒤에서 기다리고 있어요",
        async || {
            count(
                &w,
                "select count(*) from bot_user_occurrences where schedule = 'u-good'",
            )
            .await
                == 1
        },
    )
    .await;
    assert!(
        !tick.is_finished(),
        "스케줄러는 손상 행의 잠금을 기다리는 중이어야 한다"
    );
    assert_eq!(state(&w, "u-bad").await, "active:", "아직 닫히지 않았다");
    // 잠금이 풀리면 손상 행도 닫힌다
    lock.rollback().await.unwrap();
    let done = tick.await.unwrap().unwrap();
    assert_eq!((done.fired, done.closed), (1, 1));
    assert_eq!(state(&w, "u-bad").await, "closed:invalid_timing");
    assert_eq!(state(&w, "u-good").await, "closed:finished");
}

#[tokio::test]
async fn an_unreadable_destination_kind_closes_that_row_alone_as_invalid_destination() {
    let Some(w) = world().await else {
        return;
    };
    plant_once(&w, "u-odd", 120_000).await;
    plant_once(&w, "u-good", 90_000).await;
    // 목적지 열은 모두 있지만 종류가 해석되지 않는 값(스키마는 받는다): 이 한 행이 쪽 전체를 막지 않는다
    w.pg.sql(
        "update bot_user_schedules set dest_binding = 'slack', dest_kind = 'weird', dest_id = 'C9' where id = 'u-odd'",
    )
    .await;
    let before = totals(&w).await;
    let page = w.pg.store.due(None, 100).await.unwrap();
    assert_eq!(
        (
            page.schedules
                .iter()
                .map(|s| s.id.0.as_str())
                .collect::<Vec<_>>(),
            page.unreadable
                .iter()
                .map(|u| (u.id.0.as_str(), u.reason))
                .collect::<Vec<_>>(),
        ),
        (
            vec!["u-good"],
            vec![("u-odd", bot_core::schedule::CloseReason::InvalidDestination)]
        )
    );
    assert_eq!(totals(&w).await, before, "읽기는 아무것도 바꾸지 않는다");
    let scheduler = scheduler(&w);
    let tick = scheduler.tick_users().await.unwrap();
    assert_eq!((tick.fired, tick.closed), (1, 1));
    assert_eq!(state(&w, "u-odd").await, "closed:invalid_destination");
    assert_eq!(state(&w, "u-good").await, "closed:finished");
    assert_eq!(
        count(
            &w,
            "select count(*) from bot_audit where action = 'schedule.close' and target = 'u-odd' and verdict = 'invalid_destination'"
        )
        .await,
        1
    );
    let after = totals(&w).await;
    assert_eq!(after[4], before[4], "원래 대화 안내는 없다");
    for _ in 0..2 {
        let again = scheduler.tick_users().await.unwrap();
        assert_eq!((again.fired, again.closed), (0, 0));
        assert_eq!(totals(&w).await, after);
    }
}
