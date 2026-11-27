//! PostgreSQL + scheduler tests for user-schedule fairness and head-of-line blocking avoidance.
//! Verifies keyset pagination advances past held rows within one cycle, preserves held occurrences without loss, bounds recovery under continuous arrivals, and gives overdue recurring schedules one turn per pass.

use crate::support::*;
use bot_broker::testing::{FakeCatalog, snapshot};
use bot_core::{
    id::{BindingId, ProfileId},
    port::Error,
    schedule::{DeliveryTarget, ScheduleRoutes},
    state::{Conversation, ConversationKind},
};
use bot_runtime::{Scheduler, SchedulerConfig, SchedulerDeps};
use std::sync::{Arc, Mutex};

const STUCK_OWNERS: [&str; 4] = ["local:o1", "local:o2", "local:o3", "local:o4"];
const PER_OWNER: usize = 25;
const FREE: &str = "local:free";

struct Routes {
    answer: Mutex<Result<DeliveryTarget, Error>>,
    asked: Mutex<usize>,
}

impl Routes {
    fn new(answer: Result<DeliveryTarget, Error>) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            asked: Mutex::new(0),
        })
    }
    fn now_says(&self, answer: Result<DeliveryTarget, Error>) {
        *self.answer.lock().unwrap() = answer;
    }
    fn asked(&self) -> usize {
        *self.asked.lock().unwrap()
    }
}

impl ScheduleRoutes for Routes {
    fn resolve(&self, _: &ProfileId, _: &DeliveryTarget) -> Result<DeliveryTarget, Error> {
        *self.asked.lock().unwrap() += 1;
        match &*self.answer.lock().unwrap() {
            Ok(found) => Ok(found.clone()),
            Err(_) => Err(Error::Unavailable),
        }
    }
}

fn channel(id: &str) -> DeliveryTarget {
    DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: Conversation {
            kind: ConversationKind::Channel,
            id: id.into(),
            thread: Some("t1".into()),
        },
    }
}

fn live_profile() -> Arc<bot_core::profile::Snapshot> {
    let mut builder = snapshot("personal", "r1")
        .role("owner", &["run", "calendar.read"])
        .policy_scopes(&["run", "calendar.read"])
        .slack_dm_binding("desk", "main")
        .member(FREE, &["owner"]);
    for owner in STUCK_OWNERS {
        builder = builder.member(owner, &["owner"]);
    }
    builder.build()
}

fn scheduler(w: &World, routes: &Arc<Routes>, batch: usize) -> Scheduler {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(live_profile());
    Scheduler::new(
        SchedulerDeps {
            schedule: w.pg.store.clone(),
            user: w.pg.store.clone(),
            catalog,
            tracer: Arc::new(bot_core::trace::NoopTracer),
        },
        SchedulerConfig {
            interval: std::time::Duration::from_millis(10),
            batch,
        },
    )
    .with_routes(routes.clone())
}

/// Inserts 100 earlier-due schedules with explicit destinations (4 owners x 25) plus one later-due origin-conversation schedule.
async fn plant_blockers_and_one_free(w: &World) {
    let origin = w.accept_slack_dm("origin", "Origin conversation").await;
    for (k, owner) in STUCK_OWNERS.iter().enumerate() {
        w.pg.sql(&format!(
            "with t as (select bot_now_ms() as now)
             insert into bot_user_schedules
               (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
                ceiling, dest_binding, dest_kind, dest_id, dest_thread, at_ms, instruction, cursor_ms, next_ms)
             select 'personal', 's-{k}-' || lpad(i::text, 2, '0'), '{owner}', 'main', '{}', 'c-s-{k}-' || i, 'desk', 'direct', 'owner',
                    '{{run,calendar.read}}', 'slack', 'channel', 'T1/C9', 't1', now - 120000 - {k} * 1000 - i, 'Send weekly report',
                    now - 3600000, now - 120000 - {k} * 1000 - i
             from t, generate_series(1, {PER_OWNER}) as i",
            origin.0
        ))
        .await;
    }
    w.pg.sql(&format!(
        "with t as (select bot_now_ms() as now)
         insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, at_ms, instruction, cursor_ms, next_ms)
         select 'personal', 'z-free', '{FREE}', 'main', '{}', 'c-z-free', 'desk', 'direct', 'free',
                '{{run,calendar.read}}', now - 60000, 'Fifth owner schedule', now - 3600000, now - 60000
         from t",
        origin.0
    ))
    .await;
}

/// Drives one full scheduler pass while `backlog` remains true; returns `(fired, ticks)`.
async fn cycle(scheduler: &Scheduler) -> (usize, usize) {
    let (mut fired, mut ticks) = (0, 0);
    loop {
        let tick = scheduler.tick_users().await.unwrap();
        fired += tick.fired;
        ticks += 1;
        assert!(ticks <= 1000, "scheduler cycle did not terminate");
        if !tick.backlog {
            return (fired, ticks);
        }
    }
}

async fn fired_ids(w: &World) -> Vec<String> {
    sqlx::query_scalar("select schedule from bot_user_occurrences order by schedule")
        .fetch_all(w.pg.store.pool())
        .await
        .unwrap()
}

async fn stuck_fingerprint(w: &World) -> String {
    w.pg.scalar::<String>(
        "select coalesce(md5(string_agg(id || ':' || at_ms || ':' || cursor_ms || ':' || next_ms || ':' || state, ',' order by id)), '')
         from bot_user_schedules where id like 's-%'",
    )
    .await
}

#[tokio::test]
async fn held_schedules_filling_the_first_pages_do_not_block_another_owners_schedule_within_one_cycle()
 {
    for batch in [100, 1, 3] {
        let Some(w) = world().await else {
            return;
        };
        plant_blockers_and_one_free(&w).await;
        let held = stuck_fingerprint(&w).await;
        let runs = w.pg.scalar::<i64>("select count(*) from bot_runs").await;
        let routes = Routes::new(Err(Error::Unavailable));
        let scheduler = scheduler(&w, &routes, batch);

        let (fired, ticks) = cycle(&scheduler).await;
        assert_eq!(fired, 1, "batch {batch}");
        assert_eq!(fired_ids(&w).await, ["z-free"], "batch {batch}");
        assert_eq!(
            w.pg.scalar::<i64>("select count(*) from bot_runs").await,
            runs + 1,
            "batch {batch}"
        );
        assert_eq!(routes.asked(), 100, "batch {batch}");
        assert_eq!(ticks, 101usize.div_ceil(batch), "batch {batch}");
        assert_eq!(stuck_fingerprint(&w).await, held, "batch {batch}");
        assert_eq!(
            w.pg.scalar::<i64>(
                "select count(*) from bot_user_schedules where state = 'active' and id like 's-%'"
            )
            .await,
            100,
            "batch {batch}"
        );

        let (again, _) = cycle(&scheduler).await;
        assert_eq!(again, 0, "batch {batch}");
        assert_eq!(routes.asked(), 200, "batch {batch}");

        routes.now_says(Ok(channel("T1/C9")));
        let (recovered, _) = cycle(&scheduler).await;
        assert_eq!(recovered, 100, "batch {batch}");
        assert_eq!(
            w.pg.scalar::<i64>(
                "select count(*) from bot_user_occurrences o join bot_user_schedules s on s.id = o.schedule and s.profile = o.profile
                 where o.at_ms = s.at_ms and s.id like 's-%'"
            )
            .await,
            100,
            "batch {batch}"
        );
    }
}

#[tokio::test]
async fn every_due_schedule_fires_in_one_cycle_when_the_due_set_spans_several_pages() {
    let Some(w) = world().await else {
        return;
    };
    plant_blockers_and_one_free(&w).await;
    let routes = Routes::new(Ok(channel("T1/C9")));
    let (fired, ticks) = cycle(&scheduler(&w, &routes, 3)).await;
    assert_eq!(fired, 101);
    assert_eq!(ticks, 34);
    assert_eq!(fired_ids(&w).await.len(), 101);
    assert_eq!(cycle(&scheduler(&w, &routes, 3)).await, (0, 1));
}

async fn plant_row(w: &World, origin: &str, id: &str, owner: &str, dest: bool, ago: i64) {
    let (binding, kind, dest_id, thread) = if dest {
        ("'slack'", "'channel'", "'T1/C9'", "'t1'")
    } else {
        ("null", "null", "null", "null")
    };
    w.pg.sql(&format!(
        "with t as (select bot_now_ms() as now)
         insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, dest_binding, dest_kind, dest_id, dest_thread, at_ms, instruction, cursor_ms, next_ms)
         select 'personal', '{id}', '{owner}', 'main', '{origin}', 'c-{id}', 'desk', 'direct', 'owner',
                '{{run,calendar.read}}', {binding}, {kind}, {dest_id}, {thread}, now - {ago}, 'Check', now - 3600000, now - {ago}
         from t"
    ))
    .await;
}

async fn held_fired(w: &World) -> i64 {
    w.pg.scalar::<i64>("select count(*) from bot_user_occurrences where schedule like 'h-%'")
        .await
}

#[tokio::test]
async fn a_recovered_held_schedule_fires_within_a_bounded_number_of_checks_even_while_new_schedules_keep_arriving()
 {
    let Some(w) = world().await else {
        return;
    };
    let origin = w.accept_slack_dm("origin", "Origin conversation").await.0;
    for (i, id) in ["h-1", "h-2", "h-3"].into_iter().enumerate() {
        plant_row(&w, &origin, id, STUCK_OWNERS[0], true, 120_000 - i as i64).await;
    }
    plant_row(&w, &origin, "z-free", FREE, false, 60_000).await;
    let routes = Routes::new(Err(Error::Unavailable));
    let scheduler = scheduler(&w, &routes, 1);
    let first = scheduler.tick_users().await.unwrap();
    assert!(first.backlog && first.fired == 0);

    // After recovery, continuously inject new due rows to ensure the pass snapshot still wraps back to `h-1`.
    routes.now_says(Ok(channel("T1/C9")));
    let mut ticks = 1;
    for n in 0.. {
        assert!(
            ticks <= 12,
            "arrivals starved recovered schedules: {} / 3",
            held_fired(&w).await
        );
        plant_row(&w, &origin, &format!("n-{n:02}"), FREE, false, 1_000).await;
        scheduler.tick_users().await.unwrap();
        ticks += 1;
        if held_fired(&w).await == 3 {
            break;
        }
    }
    assert!(ticks <= 7, "ticks={ticks}");
    let planted = ticks - 1;
    let mut more = 0;
    while w
        .pg
        .scalar::<i64>("select count(*) from bot_user_occurrences where schedule like 'n-%'")
        .await
        < planted
    {
        scheduler.tick_users().await.unwrap();
        more += 1;
        assert!(more < 40, "newly arrived schedules did not fire");
    }
}

async fn plant_recurring(w: &World, origin: &str, id: &str, owner: &str) {
    w.pg.sql(&format!(
        "insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, cron, timezone, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{owner}', 'main', '{origin}', 'c-{id}', 'desk', 'direct', 'owner',
            '{{run,calendar.read}}', '* * * * *', 'UTC', 'Check every minute', bot_now_ms() - 1801000, bot_now_ms() - 1800000)"
    ))
    .await;
}

async fn occurrences_of(w: &World, id: &str) -> Vec<i64> {
    sqlx::query_scalar(&format!(
        "select at_ms from bot_user_occurrences where schedule = '{id}' order by at_ms"
    ))
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap()
}

async fn one_tick(w: &World, scheduler: &Scheduler, seen: &mut usize) -> (Vec<String>, bool) {
    let tick = scheduler.tick_users().await.unwrap();
    let all: Vec<String> = sqlx::query_scalar(
        "select schedule from bot_user_occurrences order by created_ms, at_ms, schedule",
    )
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap();
    let fresh = all[*seen..].to_vec();
    *seen = all.len();
    (fresh, tick.backlog)
}

/// Lists overdue healthy schedules using a 2s margin so minute boundaries mid-test do not cause flake.
async fn overdue_healthy(w: &World) -> Vec<String> {
    sqlx::query_scalar(
        "select id from bot_user_schedules where state = 'active' and next_ms <= bot_now_ms() - 2000 and dest_binding is null order by id",
    )
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn an_overdue_recurring_schedule_gets_one_turn_per_pass_and_the_others_fire_in_the_first_ticks()
 {
    let Some(w) = world().await else {
        return;
    };
    let origin = w.accept_slack_dm("origin", "Origin conversation").await.0;
    plant_recurring(&w, &origin, "r-minutely", STUCK_OWNERS[0]).await;
    plant_row(&w, &origin, "h-held", STUCK_OWNERS[0], true, 120_000).await;
    plant_row(&w, &origin, "z-free", FREE, false, 60_000).await;
    let runs = w.pg.scalar::<i64>("select count(*) from bot_runs").await;
    let routes = Routes::new(Err(Error::Unavailable));
    let scheduler = scheduler(&w, &routes, 1);
    let mut seen = 0;

    // First pass: fire recurring once, skip its re-entry in the same pass, hold `h-held`, fire `z-free`.
    let mut order = Vec::new();
    for _ in 0..4 {
        let (fresh, backlog) = one_tick(&w, &scheduler, &mut seen).await;
        order.push((fresh, backlog));
    }
    assert_eq!(
        order,
        [
            (vec!["r-minutely".to_owned()], true),
            (vec![], true),
            (vec![], true),
            (vec!["z-free".to_owned()], true),
        ]
    );
    assert!(occurrences_of(&w, "h-held").await.is_empty());
    assert_eq!(routes.asked(), 1);

    routes.now_says(Ok(channel("T1/C9")));
    let mut after = Vec::new();
    for _ in 0..3 {
        after.push(one_tick(&w, &scheduler, &mut seen).await.0);
    }
    assert_eq!(
        after,
        [
            vec!["r-minutely".to_owned()],
            vec![],
            vec!["h-held".to_owned()]
        ]
    );

    let mut ticks = 0;
    loop {
        let (_, backlog) = one_tick(&w, &scheduler, &mut seen).await;
        ticks += 1;
        assert!(ticks <= 100, "failed to catch up");
        if !backlog {
            break;
        }
    }
    assert_eq!(overdue_healthy(&w).await, Vec::<String>::new());
    let all = occurrences_of(&w, "r-minutely").await;
    assert!(all.len() >= 29, "expected ~30 occurrences: {}", all.len());
    assert!(all.windows(2).all(|pair| pair[1] - pair[0] == 60_000));
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs").await,
        runs + all.len() as i64 + 2
    );
}

#[tokio::test]
async fn a_held_row_behind_an_overdue_recurring_schedule_does_not_cut_the_catch_up_short() {
    let Some(w) = world().await else {
        return;
    };
    let origin = w.accept_slack_dm("origin", "Origin conversation").await.0;
    plant_recurring(&w, &origin, "r-minutely", STUCK_OWNERS[0]).await;
    plant_row(&w, &origin, "z-held", STUCK_OWNERS[0], true, 60_000).await;
    let routes = Routes::new(Err(Error::Unavailable));
    let scheduler = scheduler(&w, &routes, 1);
    let mut seen = 0;
    let mut rests = 0;
    for tick in 1.. {
        assert!(
            tick <= 300,
            "failed to catch up: {}",
            occurrences_of(&w, "r-minutely").await.len()
        );
        let (_, backlog) = one_tick(&w, &scheduler, &mut seen).await;
        if !backlog {
            rests = tick;
            break;
        }
    }
    assert_eq!(
        overdue_healthy(&w).await,
        Vec::<String>::new(),
        "rested at tick {rests}"
    );
    let all = occurrences_of(&w, "r-minutely").await;
    assert!(
        all.len() >= 29,
        "rested at tick {rests} after only {} occurrences",
        all.len()
    );
    assert!(all.windows(2).all(|pair| pair[1] - pair[0] == 60_000));
    assert!(occurrences_of(&w, "z-held").await.is_empty());
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_user_schedules where id = 'z-held'")
            .await,
        "active"
    );
}
