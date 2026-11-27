//! Worker claim loop, bounded concurrency, graceful drain, and shutdown release.

mod agent;
mod calls;
mod checkpoint;
mod control;
mod delivery;
mod exec;
mod session;
mod span;
#[cfg(test)]
mod tests;

pub use delivery::Delivery;

use crate::host::{Deps, Handback, Quiesce, WorkerConfig};
use bot_core::command::{Failure, Finish, Retry, RunOutcome};
use std::sync::Arc;
use tokio::{sync::watch, task::JoinSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Succeeded,
    Parked,
    Failed(Retry),
    Cancelled,
    Lost,
    Released,
}

pub(crate) enum Flow {
    Done(Finish, RunOutcome),
    /// Approved scheduled delivery was enqueued and completed atomically by the store; skip `finish`.
    Delivered,
    Parked,
    Fail(Failure),
    Lost,
    Release,
}

/// Maps interruption signals to flow transitions (lease loss > operator cancel > shutdown release).
pub(crate) fn interrupted(control: &control::Control) -> Option<Flow> {
    control.stopped().then(|| {
        if control.lost() {
            Flow::Lost
        } else if !control.cancelled() && control.shutdown() {
            Flow::Release
        } else {
            Flow::Fail(Failure::Permanent("cancelled"))
        }
    })
}

#[derive(Clone)]
pub struct Worker {
    pub(crate) deps: Arc<Deps>,
    pub(crate) cfg: Arc<WorkerConfig>,
    pub(crate) delivery: Option<Delivery>,
    pub(crate) handback: Option<Arc<dyn Handback>>,
    pub(crate) quiesce: Option<Arc<dyn Quiesce>>,
}

impl Worker {
    pub fn new(deps: Deps, cfg: WorkerConfig) -> Self {
        Self {
            deps: Arc::new(deps),
            cfg: Arc::new(cfg),
            delivery: None,
            handback: None,
            quiesce: None,
        }
    }

    #[must_use]
    pub fn with_delivery(mut self, delivery: Delivery) -> Self {
        self.delivery = Some(delivery);
        self
    }

    #[must_use]
    pub fn with_handback(mut self, handback: Arc<dyn Handback>) -> Self {
        self.handback = Some(handback);
        self
    }

    #[must_use]
    pub fn with_quiesce(mut self, quiesce: Arc<dyn Quiesce>) -> Self {
        self.quiesce = Some(quiesce);
        self
    }

    pub async fn run_once(&self) -> Option<Outcome> {
        match self.deps.runs.claim().await {
            Ok(Some(ctx)) => Some(self.execute(ctx).await),
            Ok(None) => None,
            Err(_) => {
                tracing::warn!(code = "claim_unavailable", "worker claim failed");
                None
            }
        }
    }

    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        let limit = self.cfg.concurrency.max(1);
        let mut tasks: JoinSet<Outcome> = JoinSet::new();
        let (release, release_rx) = watch::channel(false);
        'serve: loop {
            if *shutdown.borrow() {
                break;
            }
            while let Some(finished) = tasks.try_join_next() {
                log_finished(finished);
            }
            if tasks.len() < limit {
                match self.deps.runs.claim().await {
                    Ok(Some(ctx)) => {
                        let worker = self.clone();
                        let release = release_rx.clone();
                        tasks.spawn(async move { worker.execute_with(ctx, release).await });
                        continue;
                    }
                    Ok(None) => {}
                    Err(_) => tracing::warn!(code = "claim_unavailable", "worker claim failed"),
                }
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        break 'serve;
                    }
                }
                Some(finished) = tasks.join_next(), if !tasks.is_empty() => log_finished(finished),
                () = tokio::time::sleep(self.cfg.poll) => {}
            }
        }
        let drain = async {
            while let Some(finished) = tasks.join_next().await {
                log_finished(finished);
            }
        };
        if tokio::time::timeout(self.cfg.grace, drain).await.is_err() {
            tracing::warn!(
                code = "grace_exceeded",
                "worker stopped with runs in flight"
            );
            // Signal remaining tasks to stop, quiesce, and release back to the queue before aborting.
            let _ = release.send(true);
            let cleanup = async {
                while let Some(finished) = tasks.join_next().await {
                    log_finished(finished);
                }
            };
            if tokio::time::timeout(self.cfg.cleanup, cleanup)
                .await
                .is_err()
            {
                tracing::warn!(
                    code = "cleanup_exceeded",
                    "worker stopped with unfinished cleanup"
                );
            }
        }
        tasks.abort_all();
    }
}

fn log_finished(finished: Result<Outcome, tokio::task::JoinError>) {
    if let Err(error) = finished
        && error.is_panic()
    {
        tracing::error!(code = "run_panicked", "worker run panicked");
    }
}
