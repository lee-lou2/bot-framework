//! Scheduler role wiring: dispatches static cron and chat-created schedules into the durable run queue.
use super::trace::Observe;
use bot_core::{
    port::ScheduleStore,
    profile::ProfileCatalog,
    schedule::{ScheduleRoutes, UserSchedules},
};
use bot_profile::Registry;
use bot_runtime::{Scheduler, SchedulerConfig, SchedulerDeps};
use bot_store::Store;
use std::sync::Arc;
use tokio::{sync::watch, task::JoinSet};
use tokio_util::sync::CancellationToken;

pub struct Deps {
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    pub observe: Arc<Observe>,
    pub stop: CancellationToken,
    pub routes: Arc<dyn ScheduleRoutes>,
}

pub struct Prepared {
    scheduler: Scheduler,
    stop: CancellationToken,
}

pub fn prepare(deps: &Deps) -> Prepared {
    Prepared {
        scheduler: Scheduler::new(
            SchedulerDeps {
                schedule: deps.store.clone() as Arc<dyn ScheduleStore>,
                user: deps.store.clone() as Arc<dyn UserSchedules>,
                catalog: deps.registry.clone() as Arc<dyn ProfileCatalog>,
                tracer: deps.observe.tracer(),
            },
            SchedulerConfig::default(),
        )
        .with_routes(deps.routes.clone()),
        stop: deps.stop.clone(),
    }
}

impl Prepared {
    /// Starts dispatching schedules; call only after active revision and schedule definitions are committed.
    pub fn open(self, tasks: &mut JoinSet<Result<(), String>>) {
        let Self { scheduler, stop } = self;
        tasks.spawn(async move {
            let (tx, rx) = watch::channel(false);
            tokio::spawn(async move {
                stop.cancelled().await;
                let _ = tx.send(true);
            });
            scheduler.run(rx).await;
            Ok(())
        });
    }
}
