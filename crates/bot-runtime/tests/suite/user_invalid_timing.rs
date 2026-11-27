//! PostgreSQL + scheduler tests for corrupted stored user-schedule rows (`invalid_timing` and `invalid_destination`).
//! Verifies a corrupted row is closed individually without stalling healthy rows on the same page, blocking behind row locks, or mutating state during read-only `due()`.

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

async fn plant_cron(w: &World, id: &str, cron: &str, zone: &str, ago: i64) {
    let origin = w
        .accept_slack_dm(&format!("origin-{id}"), "Origin conversation")
        .await;
    w.pg.sql(&format!(
        "insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, cron, timezone, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{OWNER}', 'main', '{}', 'c-{id}', 'desk', 'direct', 'owner',
            '{{run,calendar.read}}', '{cron}', '{zone}', 'Send weekly report', bot_now_ms() - 3600000, bot_now_ms() - {ago})",
        origin.0
    ))
    .await;
}

async fn plant_once(w: &World, id: &str, ago: i64) {
    let origin = w
        .accept_slack_dm(&format!("origin-{id}"), "Origin conversation")
        .await;
    w.pg.sql(&format!(
        "insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, at_ms, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{OWNER}', 'main', '{}', 'c-{id}', 'desk', 'direct', 'owner',
            '{{run,calendar.read}}', bot_now_ms() - {ago}, 'Check', bot_now_ms() - 3600000, bot_now_ms() - {ago})",
        origin.0
    ))
    .await;
}

async fn count(w: &World, sql: &str) -> i64 {
    w.pg.scalar::<i64>(sql).await
}

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
    for (why, cron, zone, unreadable) in [
        ("unreadable stored cron", "not-a-cron", "UTC", true),
        (
            "control: readable cron + invalid timezone",
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
            "{why}"
        );

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
        assert!(round < 20, "scheduler pass did not terminate");
        let tick = scheduler.tick_users().await.unwrap();
        (fired, closed) = (fired + tick.fired, closed + tick.closed);
        if !tick.backlog {
            break;
        }
    }
    assert_eq!((fired, closed), (5, 1));
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
    plant_once(&w, "u-good", 120_000).await;
    plant_cron(&w, "u-bad", "not-a-cron", "UTC", 60_000).await;
    let mut lock = w.pg.store.pool().begin().await.unwrap();
    sqlx::query("select 1 from bot_user_schedules where id = 'u-bad' for update")
        .execute(&mut *lock)
        .await
        .unwrap();
    let scheduler = scheduler(&w);
    let tick = tokio::spawn(async move { scheduler.tick_users().await });
    eventually("healthy row must fire before lock releases", async || {
        count(
            &w,
            "select count(*) from bot_user_occurrences where schedule = 'u-good'",
        )
        .await
            == 1
    })
    .await;
    assert!(!tick.is_finished(), "scheduler should wait on u-bad lock");
    assert_eq!(state(&w, "u-bad").await, "active:");
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
    assert_eq!(totals(&w).await, before);
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
    assert_eq!(after[4], before[4]);
    for _ in 0..2 {
        let again = scheduler.tick_users().await.unwrap();
        assert_eq!((again.fired, again.closed), (0, 0));
        assert_eq!(totals(&w).await, after);
    }
}
