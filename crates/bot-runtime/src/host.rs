//! Runtime host ports and worker/scheduler configuration assembled by `bot-cli`.

use crate::model::{ModelRunner, RelayGrant};
use bot_core::{
    id::{AgentId, Attempt, ProfileId},
    ledger::CallLedger,
    port::{Broker, DelegationStore, Gateway, MemoryStore, Reply, RunStore, ScheduleStore},
    profile::ProfileCatalog,
    schedule::UserSchedules,
    trace::Tracer,
};
use std::{sync::Arc, time::Duration};

/// Resolves a model alias to a runner permitted for the profile (`None` fails the run with `unknown_model`).
pub trait Models: Send + Sync {
    fn runner(&self, profile: &ProfileId, alias: &str) -> Option<Arc<dyn ModelRunner>>;

    /// Ordered fallback aliases to try if the primary model fails before committing incompatible progress.
    fn fallbacks(&self, _profile: &ProfileId, _alias: &str) -> Vec<String> {
        Vec::new()
    }
}

/// Issues a per-attempt broker front relay token for `Loop` runners (dropping `RelayGrant` revokes only that attempt's token).
pub trait Relays: Send + Sync {
    fn open(&self, attempt: &Attempt, agent: &AgentId, alias: &str) -> Option<RelayGrant>;
}

pub struct NoRelays;

impl Relays for NoRelays {
    fn open(&self, _: &Attempt, _: &AgentId, _: &str) -> Option<RelayGrant> {
        None
    }
}

/// Optional conditional handback for `Lost` runs when this process's active set is stale; no-op for active sets or newer claims.
pub trait Handback: Send + Sync {
    fn release(&self, attempt: Attempt) -> Reply<'_, ()>;
}

/// Optional shutdown quiesce gate: verifies model/tool processes stopped and revokes session tokens before `release_for_shutdown`.
pub trait Quiesce: Send + Sync {
    fn quiesce(&self, attempt: Attempt) -> Reply<'_, ()>;
}

pub struct SingleModel(pub Arc<dyn ModelRunner>);

impl Models for SingleModel {
    fn runner(&self, _: &ProfileId, _: &str) -> Option<Arc<dyn ModelRunner>> {
        Some(self.0.clone())
    }
}

#[derive(Clone)]
pub struct Deps {
    pub runs: Arc<dyn RunStore>,
    pub memory: Arc<dyn MemoryStore>,
    pub delegation: Arc<dyn DelegationStore>,
    pub broker: Arc<dyn Broker>,
    pub ledger: Arc<dyn CallLedger>,
    pub catalog: Arc<dyn ProfileCatalog>,
    pub models: Arc<dyn Models>,
    pub relays: Arc<dyn Relays>,
    pub tracer: Arc<dyn Tracer>,
    /// Optional nudge after committing results; without it the gateway polls the outbox.
    pub gateway: Option<Arc<dyn Gateway>>,
}

#[derive(Clone)]
pub struct SchedulerDeps {
    pub schedule: Arc<dyn ScheduleStore>,
    pub user: Arc<dyn UserSchedules>,
    pub catalog: Arc<dyn ProfileCatalog>,
    pub tracer: Arc<dyn Tracer>,
}

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub concurrency: usize,
    pub poll: Duration,
    /// Keep around 1/3 of the store lease duration (default 30s).
    pub heartbeat: Duration,
    /// Consecutive heartbeat failures before treating the lease as lost.
    pub heartbeat_misses: u32,
    pub limits: crate::model::Limits,
    pub max_turns: usize,
    pub history_turns: u32,
    /// Post-shutdown grace period for in-flight runs to finish or park while heartbeats continue.
    pub grace: Duration,
    /// Post-grace budget for quiescing and `release_for_shutdown`; host shutdown timeout must exceed `grace + cleanup`.
    pub cleanup: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            concurrency: 4,
            poll: Duration::from_millis(500),
            heartbeat: Duration::from_secs(10),
            heartbeat_misses: 3,
            limits: crate::model::Limits::default(),
            max_turns: 24,
            history_turns: 20,
            grace: Duration::from_secs(20),
            cleanup: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    pub interval: Duration,
    pub batch: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            batch: 100,
        }
    }
}
