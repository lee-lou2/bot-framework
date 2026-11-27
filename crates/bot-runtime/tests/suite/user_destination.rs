//! PostgreSQL + scheduler tests for re-verifying explicit user-schedule destinations against `ScheduleRoutes` at fire time.
//! Matching routes fire atomically, missing/redirected routes close as `unrouted`, transient registry failures hold the occurrence untouched for later recovery, and origin-conversation schedules bypass the registry.

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

struct Routes {
    answer: Mutex<Result<DeliveryTarget, Error>>,
    asked: Mutex<Vec<String>>,
}

impl Routes {
    fn new(answer: Result<DeliveryTarget, Error>) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            asked: Mutex::new(Vec::new()),
        })
    }
    fn now_says(&self, answer: Result<DeliveryTarget, Error>) {
        *self.answer.lock().unwrap() = answer;
    }
    fn asked(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

impl ScheduleRoutes for Routes {
    fn resolve(&self, _: &ProfileId, target: &DeliveryTarget) -> Result<DeliveryTarget, Error> {
        self.asked
            .lock()
            .unwrap()
            .push(target.conversation.id.clone());
        match &*self.answer.lock().unwrap() {
            Ok(found) => Ok(found.clone()),
            Err(Error::Denied) => Err(Error::Denied),
            Err(Error::Invalid) => Err(Error::Invalid),
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
    snapshot("personal", "r1")
        .role("owner", &["run", "calendar.read"])
        .member(OWNER, &["owner"])
        .policy_scopes(&["run", "calendar.read"])
        .slack_dm_binding("desk", "main")
        .build()
}

fn scheduler(w: &World, routes: Option<Arc<Routes>>) -> Scheduler {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(live_profile());
    let scheduler = Scheduler::new(
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
    );
    match routes {
        Some(routes) => scheduler.with_routes(routes),
        None => scheduler,
    }
}

async fn plant(w: &World, id: &str, destination: Option<&str>) {
    let origin = w
        .accept_slack_dm(&format!("origin-{id}"), "Origin conversation")
        .await;
    let (binding, kind, dest) = match destination {
        Some(dest) => ("'slack'", "'channel'", format!("'{dest}'")),
        None => ("null", "null", "null".to_owned()),
    };
    let thread = if destination.is_some() {
        "'t1'"
    } else {
        "null"
    };
    w.pg.sql(&format!(
        "insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, dest_binding, dest_kind, dest_id, dest_thread, at_ms, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{OWNER}', 'main', '{}', 'c-{id}', 'desk', 'direct', 'owner',
            '{{run,calendar.read}}', {binding}, {kind}, {dest}, {thread}, bot_now_ms() - 120000, 'Send weekly report',
            bot_now_ms() - 3600000, bot_now_ms() - 120000)",
        origin.0
    ))
    .await;
}

async fn runs(w: &World) -> i64 {
    w.pg.scalar::<i64>("select count(*) from bot_runs").await
}

async fn audit(w: &World, action: &str) -> i64 {
    w.pg.scalar::<i64>(&format!(
        "select count(*) from bot_audit where action = '{action}'"
    ))
    .await
}

#[tokio::test]
async fn a_destination_that_is_still_the_registered_one_fires_with_the_destination_fixed_on_the_new_run()
 {
    let Some(w) = world().await else {
        return;
    };
    plant(&w, "s1", Some("T1/C9")).await;
    let before = runs(&w).await;
    let routes = Routes::new(Ok(channel("T1/C9")));
    let tick = scheduler(&w, Some(routes.clone()))
        .tick_users()
        .await
        .unwrap();
    assert_eq!((tick.fired, tick.closed), (1, 0));
    assert_eq!(routes.asked(), 1);
    assert_eq!(runs(&w).await, before + 1);
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_user_occurrences")
            .await,
        1
    );
    assert_eq!(
        w.pg.scalar::<String>(
            "select dest_binding || '/' || dest_kind || '/' || dest_id || '/' || dest_thread || '/' || state from bot_deliveries"
        )
        .await,
        "slack/channel/T1/C9/t1/pending"
    );
    assert_eq!(audit(&w, "schedule.fire").await, 1);
    assert_eq!(
        w.pg.scalar::<String>("select verdict from bot_audit where action = 'schedule.close'")
            .await,
        "finished"
    );
}

#[tokio::test]
async fn a_destination_that_left_or_moved_closes_the_schedule_and_fires_nothing() {
    for (answer, why) in [
        (Err(Error::Denied), "removed from registry"),
        (Err(Error::Invalid), "invalid route"),
        (Ok(channel("T2/C9")), "redirected to another team"),
        (Ok(channel("T1/C1")), "redirected to another channel"),
    ] {
        let Some(w) = world().await else {
            return;
        };
        plant(&w, "s1", Some("T1/C9")).await;
        let before = runs(&w).await;
        let tick = scheduler(&w, Some(Routes::new(answer)))
            .tick_users()
            .await
            .unwrap();
        assert_eq!((tick.fired, tick.closed), (0, 1), "{why}");
        assert_eq!(runs(&w).await, before, "{why}");
        for table in ["bot_deliveries", "bot_user_occurrences"] {
            assert_eq!(
                w.pg.scalar::<i64>(&format!("select count(*) from {table}"))
                    .await,
                0,
                "{why}: {table}"
            );
        }
        assert_eq!(
            w.pg.scalar::<String>("select state from bot_user_schedules")
                .await,
            "closed",
            "{why}"
        );
        assert_eq!(audit(&w, "schedule.close").await, 1, "{why}");
        assert_eq!(audit(&w, "schedule.fire").await, 0, "{why}");
        assert_eq!(
            w.pg.scalar::<String>("select verdict from bot_audit where action = 'schedule.close'")
                .await,
            "unrouted",
            "{why}"
        );
        let again = scheduler(&w, Some(Routes::new(Ok(channel("T1/C9")))))
            .tick_users()
            .await
            .unwrap();
        assert_eq!((again.fired, again.closed), (0, 0), "{why}");
        assert_eq!(runs(&w).await, before, "{why}");
    }
}

#[tokio::test]
async fn an_unconfirmed_registry_holds_the_occurrence_untouched_and_the_same_occurrence_fires_after_it_recovers()
 {
    let Some(w) = world().await else {
        return;
    };
    plant(&w, "s1", Some("T1/C9")).await;
    let before = runs(&w).await;
    let at =
        w.pg.scalar::<i64>("select at_ms from bot_user_schedules")
            .await;
    let (cursor, next) = (
        w.pg.scalar::<i64>("select cursor_ms from bot_user_schedules")
            .await,
        w.pg.scalar::<i64>("select next_ms from bot_user_schedules")
            .await,
    );
    let routes = Routes::new(Err(Error::Unavailable));
    for scheduler in [
        scheduler(&w, Some(routes.clone())),
        scheduler(&w, None),
        scheduler(&w, Some(routes.clone())),
    ] {
        let tick = scheduler.tick_users().await.unwrap();
        assert_eq!((tick.fired, tick.closed, tick.waiting), (0, 0, 0));
    }
    assert_eq!(runs(&w).await, before);
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_user_schedules")
            .await,
        "active"
    );
    assert_eq!(
        (
            w.pg.scalar::<i64>("select cursor_ms from bot_user_schedules")
                .await,
            w.pg.scalar::<i64>("select next_ms from bot_user_schedules")
                .await
        ),
        (cursor, next)
    );
    for action in ["schedule.fire", "schedule.close"] {
        assert_eq!(audit(&w, action).await, 0, "{action}");
    }
    routes.now_says(Ok(channel("T1/C9")));
    let tick = scheduler(&w, Some(routes)).tick_users().await.unwrap();
    assert_eq!(tick.fired, 1);
    assert_eq!(
        w.pg.scalar::<i64>("select at_ms from bot_user_occurrences")
            .await,
        at
    );
    assert_eq!(runs(&w).await, before + 1);
}

#[tokio::test]
async fn a_schedule_for_the_original_conversation_never_asks_the_registry_and_a_hold_does_not_block_the_others()
 {
    let Some(w) = world().await else {
        return;
    };
    plant(&w, "s1-stuck", Some("T1/C9")).await;
    plant(&w, "s2-plain", None).await;
    let before = runs(&w).await;
    let routes = Routes::new(Err(Error::Unavailable));
    let tick = scheduler(&w, Some(routes.clone()))
        .tick_users()
        .await
        .unwrap();
    assert_eq!((tick.fired, tick.closed), (1, 0));
    assert_eq!(routes.asked(), 1);
    assert_eq!(runs(&w).await, before + 1);
    assert_eq!(
        w.pg.scalar::<String>("select schedule from bot_user_occurrences")
            .await,
        "s2-plain"
    );
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_user_schedules where id = 's1-stuck'")
            .await,
        "active"
    );
    plant(&w, "s3-plain", None).await;
    let none = scheduler(&w, None).tick_users().await.unwrap();
    assert_eq!(none.fired, 1);
}
