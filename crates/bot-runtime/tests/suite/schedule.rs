//! 스케줄러 종단 시험: 실제 PG 저장소 위에서 놓친 발생을 따라잡고, 두 스케줄러가 한 run으로 수렴하며, 접수된 예약 run을 워커가 실행한다.

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

/// 방금 등록한 트리거를 한 시간 전부터 있던 것처럼 되돌린다(잠을 자지 않고 놓친 발생을 만든다).
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

    // 두 스케줄러가 동시에 점검해도 발생마다 run은 하나다
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
        "한 시간 치 10분 발생: {}",
        times.len()
    );
    assert!(
        times.windows(2).all(|pair| pair[1] - pair[0] == 600_000),
        "빠짐 없이 10분 간격으로 오래된 것부터"
    );
    assert!(times.iter().all(|t| t % 600_000 == 0), "정각 기준 10분");
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs").await,
        times.len() as i64,
        "발생마다 run 하나(두 스케줄러가 합쳐졌다)"
    );
    assert_eq!(
        w.pg.scalar::<i64>("select count(distinct run) from bot_occurrences")
            .await,
        times.len() as i64
    );
    // 다 따라잡았다: 다시 점검해도 새 접수가 없다
    assert_eq!(one.tick().await.unwrap().accepted, 0);

    // 접수된 예약 run은 워커가 도구를 직접 불러 끝낸다(서비스 주체의 권한으로, 승인 없이 읽기)
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
    assert_eq!(tick.accepted, 1, "놓친 발생이 여럿이어도 가장 최근 하나만");
    assert_eq!(w.pg.scalar::<i64>("select count(*) from bot_runs").await, 1);
    // 커서가 그 발생으로 건너뛰었으므로 같은 발생을 다시 접수하지 않는다
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
    until("따라잡기가 끝나야 한다", async || {
        w.pg.scalar::<i64>("select count(*) from bot_occurrences")
            .await
            >= 5
    })
    .await;
    stop.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), serving)
        .await
        .expect("종료 신호 뒤 곧 돌아와야 한다")
        .unwrap();
    // 새 판본을 게시하면 새 구간이 시작되고(커서는 그대로), 다음 발생은 새 판본으로 접수된다
    let mut next = every_ten_minutes(false);
    next.revision = bot_core::id::Revision("r2".into());
    publish(&w, next).await;
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_schedule_segments")
            .await,
        2,
        "이전 구간은 닫히고 새 구간이 열렸다"
    );
}
