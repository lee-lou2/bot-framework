//! End-to-end static cron scheduler tests over PostgreSQL: missed-occurrence catch-up, multi-scheduler convergence, beat collapse, and revision segment rollover.

use crate::support::*;
use bot_core::{
    command::ScheduleSet, id::Revision, port::AuditLog, profile::ActiveRevisions,
    profile::TriggerSource,
};
use bot_runtime::{Outcome, Scheduler, SchedulerConfig, SchedulerDeps, SingleModel, schedule_set};
use std::{collections::BTreeMap, sync::Arc};

fn every_ten_minutes(beat: bool) -> ScheduleSet {
    let mut snapshot = (*profile()).clone();
    for agent in snapshot.agents.values_mut() {
        for trigger in &mut agent.triggers {
            if let TriggerSource::Cron {
                schedule,
                beat: is_beat,
                ..
            } = &mut trigger.on
            {
                *schedule = "*/10 * * * *".into();
                *is_beat = beat;
            }
        }
    }
    schedule_set(&snapshot)
}

async fn publish(w: &World, set: ScheduleSet) {
    let store = &w.pg.store;
    let next = Revision(format!("static-{}", set.revision.0));
    store
        .replace(
            store.current().await.unwrap(),
            next,
            BTreeMap::from([(set.profile.clone(), set.revision)]),
            vec![],
            BTreeMap::from([(set.profile, set.triggers)]),
        )
        .await
        .unwrap();
}

fn scheduler(store: &Arc<bot_store::Store>) -> Scheduler {
    Scheduler::new(
        SchedulerDeps {
            schedule: store.clone(),
            user: store.clone(),
            catalog: Arc::new(bot_broker::testing::FakeCatalog::new()),
            tracer: Arc::new(bot_core::trace::NoopTracer),
        },
        SchedulerConfig {
            interval: std::time::Duration::from_millis(10),
            batch: 100,
        },
    )
}

async fn backdate(w: &World) {
    w.pg.sql("update bot_schedule_segments set from_ms = from_ms - 3600000")
        .await;
    w.pg.sql("update bot_schedule_cursors set cursor_ms = cursor_ms - 3600000")
        .await;
}

#[tokio::test]
async fn catches_up_missed_schedule() {
    let Some(w) = world().await else {
        return;
    };
    publish(&w, every_ten_minutes(false)).await;
    backdate(&w).await;

    let peer = w.pg.peer("scheduler-2").await;
    let (one, two) = (scheduler(&w.pg.store), scheduler(&peer));
    let (a, b) = tokio::join!(one.tick(), two.tick());
    assert!(a.is_ok() && b.is_ok());
    let times: Vec<i64> = sqlx::query_scalar("select at_ms from bot_occurrences order by at_ms")
        .fetch_all(w.pg.store.pool())
        .await
        .unwrap();
    assert!(
        (5..=7).contains(&times.len()),
        "expected 5..=7 ten-minute ticks in one hour: {}",
        times.len()
    );
    assert!(times.windows(2).all(|pair| pair[1] - pair[0] == 600_000));
    assert!(times.iter().all(|t| t % 600_000 == 0));
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs").await,
        times.len() as i64
    );
    assert_eq!(
        w.pg.scalar::<i64>("select count(distinct run) from bot_occurrences")
            .await,
        times.len() as i64
    );
    assert_eq!(one.tick().await.unwrap().accepted, 0);

    let worker = w.worker(
        &w.pg.store,
        Arc::new(SingleModel(Arc::new(bot_runtime::model::DemoModel))),
        config(),
    );
    for _ in 0..times.len() {
        assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
    }
    assert_eq!(worker.run_once().await, None);
    assert_eq!(w.runner.jobs().len(), times.len());
    assert_eq!(
        w.pg.scalar::<i64>(
            "select count(*) from bot_runs where state = 'succeeded' and trigger = 'morning'"
        )
        .await,
        times.len() as i64
    );
    assert_eq!(w.pg.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_beat_catches_up_with_only_its_latest_occurrence() {
    let Some(w) = world().await else {
        return;
    };
    publish(&w, every_ten_minutes(true)).await;
    backdate(&w).await;
    let tick = scheduler(&w.pg.store).tick().await.unwrap();
    assert_eq!(tick.accepted, 1);
    assert_eq!(w.pg.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(scheduler(&w.pg.store).tick().await.unwrap().accepted, 0);
}

#[tokio::test]
async fn the_loop_serves_until_shutdown_and_a_new_revision_starts_a_new_segment() {
    let Some(w) = world().await else {
        return;
    };
    publish(&w, every_ten_minutes(false)).await;
    backdate(&w).await;
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let serving = {
        let s = scheduler(&w.pg.store);
        tokio::spawn(async move { s.run(shutdown).await })
    };
    until("catch-up must finish", async || {
        w.pg.scalar::<i64>("select count(*) from bot_occurrences")
            .await
            >= 5
    })
    .await;
    stop.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), serving)
        .await
        .expect("scheduler must exit on shutdown")
        .unwrap();
    let mut next = every_ten_minutes(false);
    next.revision = bot_core::id::Revision("r2".into());
    publish(&w, next).await;
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_schedule_segments")
            .await,
        2
    );
}
