//! PostgreSQL tests for conditional handback (`release_stale`) when a worker steps back after activation replacement.
//! Handback immediately returns the run to `ready` with zero attempts spent, while leaving current-activation or already-taken-over claims untouched.

use crate::support::*;
use bot_core::{
    command::Finish,
    id::*,
    port::{Error, Reply, RunStore},
    profile::ActiveRevisions,
    schedule::{DeliveryDraft, ScheduledReplies},
};
use bot_runtime::{Delivery, Handback, Models, Outcome, SingleModel, model::DemoModel};
use bot_store::{Config, Store};
use std::{collections::BTreeMap, sync::Arc};

fn demo() -> Arc<dyn Models> {
    Arc::new(SingleModel(Arc::new(DemoModel)))
}

fn rev(value: &str) -> Revision {
    Revision(value.into())
}

struct ToStore(Arc<Store>);

impl Handback for ToStore {
    fn release(&self, attempt: Attempt) -> Reply<'_, ()> {
        Box::pin(async move { self.0.release_stale(&attempt).await })
    }
}

/// Replaces the active set right before reading the delivery draft so the old handle sees `Stale`.
struct ReplacedBeforeTheDraft {
    world: Arc<Store>,
    inner: Arc<Store>,
    done: std::sync::atomic::AtomicBool,
}

impl ScheduledReplies for ReplacedBeforeTheDraft {
    fn receipt(&self, attempt: Attempt) -> Reply<'_, Option<bot_core::command::JsonPayload>> {
        ScheduledReplies::receipt(&*self.inner, attempt)
    }

    fn draft(
        &self,
        attempt: Attempt,
        completion: Option<Finish>,
    ) -> Reply<'_, Option<DeliveryDraft>> {
        Box::pin(async move {
            if !self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.world
                    .replace(
                        Some(rev("set-1")),
                        rev("set-2"),
                        BTreeMap::from([(ProfileId("personal".into()), rev("r1"))]),
                        Vec::new(),
                        Default::default(),
                    )
                    .await?;
            }
            ScheduledReplies::draft(&*self.inner, attempt, completion).await
        })
    }

    fn target(&self, attempt: Attempt) -> Reply<'_, Option<bot_core::schedule::DeliveryTarget>> {
        ScheduledReplies::target(&*self.inner, attempt)
    }

    fn finish_unsent(
        &self,
        fence: bot_core::schedule::DeliveryFence,
        target: bot_core::schedule::DeliveryTarget,
        reason: bot_core::schedule::UnsentReason,
    ) -> Reply<'_, bot_core::command::JsonPayload> {
        ScheduledReplies::finish_unsent(&*self.inner, fence, target, reason)
    }

    fn enqueue(&self, approval: bot_core::ledger::StartCall) -> Reply<'_, ()> {
        ScheduledReplies::enqueue(&*self.inner, approval)
    }
}

async fn as_scheduled(w: &World, run: &RunId) {
    w.pg.sql(&format!(
        "update bot_runs set run_origin = 'schedule' where id = '{}'",
        run.0
    ))
    .await;
}

fn handle(w: &World, worker: &str, set: &str) -> Arc<Store> {
    Arc::new(
        Store::from_pool(
            w.pg.store.pool().clone(),
            Config {
                worker: worker.into(),
                max_attempts: 1,
                ..Config::default()
            },
        )
        .for_activation(rev(set)),
    )
}

async fn attempts(w: &World, run: &RunId) -> i32 {
    w.pg.scalar::<i32>(&format!(
        "select attempts from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

fn replaced_before_the_draft(w: &World, old: &Arc<Store>) -> Arc<ReplacedBeforeTheDraft> {
    Arc::new(ReplacedBeforeTheDraft {
        world: w.pg.store.clone(),
        inner: old.clone(),
        done: Default::default(),
    })
}

#[tokio::test]
async fn a_worker_that_steps_back_after_the_set_was_replaced_hands_the_run_back_and_a_new_worker_finishes_it()
 {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-1", "Show today's agenda").await;
    as_scheduled(&w, &run).await;
    let old = handle(&w, "old-worker", "set-1");
    let worker = w
        .worker(&old, demo(), config())
        .with_delivery(Delivery {
            replies: replaced_before_the_draft(&w, &old),
            release: w.broker.clone(),
        })
        .with_handback(Arc::new(ToStore(old.clone())));
    assert_eq!(worker.run_once().await, Some(Outcome::Lost));
    assert_eq!(w.pg.state_of(&run).await, "ready");
    assert_eq!(attempts(&w, &run).await, 0);
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.handover'")
            .await,
        1
    );
    let new = handle(&w, "new-worker", "set-2");
    let resumed = w.worker(&new, demo(), config());
    assert_eq!(resumed.run_once().await, Some(Outcome::Succeeded));
    assert_eq!(w.pg.state_of(&run).await, "succeeded");
    assert_eq!(attempts(&w, &run).await, 0);
}

#[tokio::test]
async fn without_the_handback_the_run_stays_leased_until_the_lease_ends_and_the_set_change_spares_the_attempt()
 {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-2", "Show today's agenda").await;
    as_scheduled(&w, &run).await;
    let old = handle(&w, "old-worker", "set-1");
    let worker = w.worker(&old, demo(), config()).with_delivery(Delivery {
        replies: replaced_before_the_draft(&w, &old),
        release: w.broker.clone(),
    });
    assert_eq!(worker.run_once().await, Some(Outcome::Lost));
    assert_eq!(w.pg.state_of(&run).await, "leased");
    w.pg.sql(&format!(
        "update bot_runs set lease_until_ms = bot_now_ms() - 1 where id = '{}'",
        run.0
    ))
    .await;
    let new = handle(&w, "new-worker", "set-2");
    let taken = RunStore::claim(&*new)
        .await
        .unwrap()
        .expect("cross-activation takeover must spare attempts");
    assert_eq!(taken.attempt.run, run);
    assert_eq!(attempts(&w, &run).await, 0);
}

#[tokio::test]
async fn a_run_already_taken_over_or_held_by_a_current_worker_is_left_alone() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-3", "Show today's agenda").await;
    let current = handle(&w, "current-worker", "set-1");
    let claimed = RunStore::claim(&*current).await.unwrap().expect("claim");
    assert!(matches!(
        current.release_stale(&claimed.attempt).await,
        Err(Error::Denied)
    ));
    assert_eq!(w.pg.state_of(&run).await, "leased");

    w.pg.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            BTreeMap::from([(ProfileId("personal".into()), rev("r1"))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    w.pg.sql(&format!(
        "update bot_runs set lease_until_ms = bot_now_ms() - 1 where id = '{}'",
        run.0
    ))
    .await;
    let successor = Arc::new(
        Store::from_pool(w.pg.store.pool().clone(), Config::default()).for_activation(rev("set-2")),
    );
    let taken = RunStore::claim(&*successor)
        .await
        .unwrap()
        .expect("takeover");
    assert!(taken.attempt.generation.0 > claimed.attempt.generation.0);
    assert!(matches!(
        current.release_stale(&claimed.attempt).await,
        Err(Error::Stale)
    ));
    assert_eq!(w.pg.state_of(&run).await, "leased");
}
