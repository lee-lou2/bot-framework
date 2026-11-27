//! `Broker` implementation gating all model-to-tool execution, approvals, and auditing.
//!
//! Call sequence: generation check -> declaration/policy lookup -> argument sealing (validate, canonicalize, hash) ->
//! idempotent call record -> prior-attempt recovery (if needed) -> authorization -> rate/concurrency limit ->
//! classification -> approval -> commit start -> execute -> record outcome.
//! Approval consumption and `start` commit in one ledger transaction; the runner is invoked only after `start` commits.
//! Pending approvals return `Awaiting` without executing; resumed runs re-evaluate current policy and consume the approval on the same call ID.

mod authority;
mod call;
mod decide;
mod deliver;
mod exec;
mod gate;
mod manage;
pub(crate) mod memory;
pub(crate) mod pass;
pub mod preflight;
mod route;
pub(crate) mod schedule;
pub(crate) mod share;
mod site;
#[cfg(test)]
mod tests;
mod wake;

use crate::{
    ports::*,
    rate::{Limiter, RateConfig},
    sys::{Clock, Entropy},
};
use bot_core::{
    command::*,
    id::*,
    ledger::CallLedger,
    manage::ProfileManager,
    memory::Embedder,
    port::{AuditLog, Broker, EffectJournal, Error, MemoryStore, ModelCall, Reply},
    profile::ProfileCatalog,
    schedule::{ScheduleRoutes, ScheduledReplies, UserSchedules},
    trace::Tracer,
};
use std::{sync::Arc, time::Duration};

pub struct Parts {
    pub catalog: Arc<dyn ProfileCatalog>,
    pub ledger: Arc<dyn CallLedger>,
    pub effects: Arc<dyn EffectJournal>,
    pub memory: Arc<dyn MemoryStore>,
    pub audit: Arc<dyn AuditLog>,
    pub runner: Arc<dyn ToolRunner>,
    pub sealer: Arc<dyn Sealer>,
    pub tracer: Arc<dyn Tracer>,
    pub clock: Arc<dyn Clock>,
    pub entropy: Arc<dyn Entropy>,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub approval_ttl: Duration,
    pub rate: RateConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            approval_ttl: Duration::from_secs(30 * 60),
            rate: RateConfig::default(),
        }
    }
}

pub struct BrokerService {
    parts: Parts,
    limiter: Arc<Limiter>,
    cfg: Config,
    /// User-created schedule store (`schedule.*` and tool wakes); defaults to returning `Unavailable`.
    schedules: Arc<dyn UserSchedules>,
    /// Frozen scheduled reply drafts and outbox release store; defaults to returning `Unavailable`.
    replies: Arc<dyn ScheduledReplies>,
    /// Host route registry resolving explicit destinations; when absent, cross-destination schedules are rejected.
    routes: Option<Arc<dyn ScheduleRoutes>>,
    /// Super-admin runtime skill/MCP manager; when absent, `skill.*` and `mcp.*` return `Unavailable`.
    manager: Option<Arc<dyn ProfileManager>>,
    /// Optional host embedding provider for `memory.search` and `memory.save`; absent falls back to lexical.
    embedder: Option<Arc<dyn Embedder>>,
}

impl BrokerService {
    pub fn new(parts: Parts, cfg: Config) -> Self {
        let limiter = Limiter::new(cfg.rate, Arc::clone(&parts.clock));
        Self {
            parts,
            limiter,
            cfg,
            schedules: Arc::new(schedule::NoSchedules),
            replies: Arc::new(deliver::NoReplies),
            routes: None,
            manager: None,
            embedder: None,
        }
    }

    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    pub fn with_manager(mut self, manager: Arc<dyn ProfileManager>) -> Self {
        self.manager = Some(manager);
        self
    }

    /// Attaches the chat-schedule store. Without it, `schedule.*` returns `Unavailable` and tool wake
    /// requests (`_meta["bot/wake"]`) hold the call rather than falsely claiming the wake was scheduled.
    pub fn with_schedules(mut self, schedules: Arc<dyn UserSchedules>) -> Self {
        self.schedules = schedules;
        self
    }

    /// Attaches the frozen reply draft store (worker brokers only; approval-decision brokers do not need it).
    pub fn with_replies(mut self, replies: Arc<dyn ScheduledReplies>) -> Self {
        self.replies = replies;
        self
    }

    /// Attaches the host route registry for resolving and verifying explicit destinations.
    pub fn with_routes(mut self, routes: Arc<dyn ScheduleRoutes>) -> Self {
        self.routes = Some(routes);
        self
    }
}

impl Broker for BrokerService {
    fn call(&self, call: ToolCall) -> Reply<'_, CallResult> {
        Box::pin(self.call_flow(call))
    }

    fn call_for_model(&self, call: ToolCall) -> Reply<'_, ModelCall> {
        Box::pin(async move {
            let error = match self.call(call.clone()).await {
                Ok(result) => return Ok(ModelCall::Returned(result)),
                // Workers re-check leases and revisions on `Stale`; never stamp a permanent refusal marker.
                Err(Error::Stale) => return Err(Error::Stale),
                Err(error) => error,
            };
            let Some(reason) = preflight::expected(&error) else {
                return Err(error);
            };
            match preflight::confirm(self.parts.ledger.as_ref(), Some(self), call, reason).await {
                Ok(true) => Ok(ModelCall::Refused(reason)),
                Ok(false) => Err(error),
                Err(Error::Stale) => Err(Error::Stale),
                Err(_) => Err(Error::Unavailable),
            }
        })
    }

    fn decide(&self, nonce: ApprovalId, actor: PrincipalId, allow: bool) -> Reply<'_, ()> {
        Box::pin(self.decide_flow(nonce, actor, allow))
    }
}
