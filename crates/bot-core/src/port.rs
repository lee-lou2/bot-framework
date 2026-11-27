//! Core cross-crate port traits for storage, secrets, broker, gateway, scheduling, delegation, effects, and audit.
//!
//! Implementers re-read and authorize state at each boundary. `Reply<'a, T>` is an async return alias, not a chat reply.
use crate::{
    command::*,
    id::*,
    intake::{RequestKey, RequestState, RunProgress},
    state::*,
};
use std::{future::Future, path::Path, pin::Pin};

#[derive(Debug)]
pub enum Error {
    Invalid,
    Denied,
    Conflict,
    Stale,
    Missing,
    Unavailable,
    RateLimited,
}

pub type Reply<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

pub trait ProfileSource: Send + Sync {
    fn validate<'a>(&'a self, root: &'a Path) -> Reply<'a, Revision>;
}

pub trait RunStore: Send + Sync {
    fn accept(&self, command: Accept) -> Reply<'_, RunId> {
        self.accept_with(command, AcceptExtras::default())
    }

    /// Persists adapter-verified extras (`AcceptExtras`) in the same commit as intake.
    ///
    /// Implementers must override `accept_with`.
    ///
    /// ```compile_fail,E0046
    /// # use bot_core::{command::*, id::*, intake::{RequestKey, RequestState, RunProgress}, port::{Reply, RunStore}, state::RunContext};
    /// # struct Store;
    /// impl RunStore for Store {
    ///     fn accept(&self, command: Accept) -> Reply<'_, RunId> { unimplemented!() }
    /// #   fn status(&self, _: RequestKey) -> Reply<'_, Option<RequestState>> { unimplemented!() }
    /// #   fn progress(&self, _: ProfileId, _: RunId) -> Reply<'_, Option<RunProgress>> { unimplemented!() }
    /// #   fn claim(&self) -> Reply<'_, Option<RunContext>> { unimplemented!() }
    /// #   fn release_for_shutdown(&self, _: Attempt) -> Reply<'_, ()> { unimplemented!() }
    /// #   fn renew(&self, _: Attempt) -> Reply<'_, Beat> { unimplemented!() }
    /// #   fn checkpoint_batch(&self, _: Attempt, _: Vec<Step>) -> Reply<'_, ()> { unimplemented!() }
    /// #   fn finish(&self, _: Attempt, _: Finish) -> Reply<'_, Completion> { unimplemented!() }
    /// #   fn finish_with(&self, _: Attempt, _: Finish, _: RunOutcome) -> Reply<'_, Completion> { unimplemented!() }
    /// #   fn fail(&self, _: Attempt, _: Failure) -> Reply<'_, Retry> { unimplemented!() }
    /// #   fn history(&self, _: Attempt, _: u32) -> Reply<'_, Vec<Turn>> { unimplemented!() }
    /// }
    /// ```
    fn accept_with(&self, command: Accept, extras: AcceptExtras) -> Reply<'_, RunId>;
    fn status(&self, key: RequestKey) -> Reply<'_, Option<RequestState>>;
    fn progress(&self, profile: ProfileId, run: RunId) -> Reply<'_, Option<RunProgress>>;
    /// Claims a ready run and records the host active set in the same transaction.
    /// On lease-expiry handover, preserves attempt count only when previous and current active sets both exist and differ.
    /// Handover bumps generation and invalidates prior approvals; resuming from voluntary approval wait preserves generation.
    fn claim(&self) -> Reply<'_, Option<RunContext>>;
    /// Called after shutdown grace once child processes and sessions for this claim are reaped.
    /// Returns a non-cancelled run to ready without spending an attempt if lease, generation, and `claim_epoch` still match;
    /// bumps generation and invalidates old approvals in one commit (next claim preserves generation), or returns `Stale`.
    fn release_for_shutdown(&self, attempt: Attempt) -> Reply<'_, ()>;
    /// Extends lease if owner, generation, and expiry still match; otherwise `Stale`.
    fn renew(&self, attempt: Attempt) -> Reply<'_, Beat>;
    fn checkpoint(&self, attempt: Attempt, step: Step) -> Reply<'_, ()> {
        self.checkpoint_batch(attempt, vec![step])
    }
    /// Atomically checkpoints steps for the current claim; idempotent on identical keys, rolls back all on content mismatch.
    fn checkpoint_batch(&self, attempt: Attempt, steps: Vec<Step>) -> Reply<'_, ()>;
    /// Atomically records result, reply outbox, and parent wake; if already cancelled, drops replies and returns cancelled status.
    fn finish(&self, attempt: Attempt, done: Finish) -> Reply<'_, Completion>;
    /// Same as `finish`, recording execution metrics (`RunOutcome`) in the same commit.
    fn finish_with(
        &self,
        attempt: Attempt,
        done: Finish,
        outcome: RunOutcome,
    ) -> Reply<'_, Completion>;
    /// Retries transient failures with backoff; dead-letters permanent failures or exhausted attempts.
    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry>;
    /// Recent turns in the same conversation (oldest first), filtered to requester visibility.
    fn history(&self, attempt: Attempt, limit: u32) -> Reply<'_, Vec<Turn>>;
}

/// Operator controls; all actions are audited.
pub trait RunAdmin: Send + Sync {
    /// Resumes a dead-letter run while keeping its logical run and effect keys, resetting attempts only.
    fn resume(
        &self,
        profile: ProfileId,
        run: RunId,
        actor: PrincipalId,
        reason: String,
    ) -> Reply<'_, ()>;
    /// Requests cancellation for the run and its children; stops active runs before the next external effect.
    fn cancel(
        &self,
        profile: ProfileId,
        run: RunId,
        actor: PrincipalId,
        reason: String,
    ) -> Reply<'_, ()>;
}

/// Reads atomically record `private_memory_read` on the request tree before returning data to the model.
pub trait MemoryStore: Send + Sync {
    fn search(&self, attempt: Attempt, query: String) -> Reply<'_, Vec<MemoryHit>>;
    /// Fetches one entry under `search` visibility rules, recording `private_memory_read` before returning private content.
    fn preview(&self, attempt: Attempt, id: MemoryId) -> Reply<'_, MemoryPreview>;
    fn change(&self, attempt: Attempt, change: MemoryChange) -> Reply<'_, MemoryId>;
    /// Atomically verifies approval, owner, and content digest, then consumes approval, shares, completes the call, and audits;
    /// digest mismatch returns `Conflict` and retries of completed calls do not alter visibility.
    fn share(&self, start: crate::ledger::StartCall, id: MemoryId) -> Reply<'_, MemoryId>;
    fn seed(&self, seed: MemorySeed) -> Reply<'_, ()>;
}

/// Profile-scoped secret handle; callers never pass paths or profile IDs.
pub trait ProfileSecrets: Send + Sync {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue>;
}

/// Deliberately omits `Debug` and `Display` to prevent secret leakage in diagnostics.
pub struct SecretValue(pub Vec<u8>);

pub trait SecretStore: Send + Sync {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>>;
}

/// Result of a model-planned call, or a pre-execution refusal verified against ledger, inputs, and limits.
pub enum ModelCall {
    Returned(CallResult),
    Refused(crate::cli::CliRefusal),
}

pub trait Broker: Send + Sync {
    fn call(&self, call: ToolCall) -> Reply<'_, CallResult>;
    /// Executes a validated model-planned call; non-refusal failures return outer `Err`.
    /// Decide-only implementations must not start execution.
    fn call_for_model(&self, _: ToolCall) -> Reply<'_, ModelCall> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn decide(&self, nonce: ApprovalId, actor: PrincipalId, allow: bool) -> Reply<'_, ()>;
}

pub trait Gateway: Send + Sync {
    fn deliver(&self, run: RunId, profile: ProfileId) -> Reply<'_, ()>;
}

pub trait ScheduleStore: Send + Sync {
    /// Atomically commits occurrence intake and cursor advance; duplicate `(profile, agent, trigger, scheduled_at)` returns the existing run.
    fn accept_occurrence(&self, occurrence: Occurrence, command: Accept) -> Reply<'_, RunId>;
    fn snapshot(&self) -> Reply<'_, ScheduleSnapshot>;
}

pub trait DelegationStore: Send + Sync {
    /// Reads a completed child result matching current claim, revision, target, instruction, and scopes;
    /// returns `None` without creating a delegation or waiting.
    fn result(&self, command: Delegate) -> Reply<'_, Option<JsonPayload>>;
    fn delegate(&self, command: Delegate) -> Reply<'_, RunId>;
}

pub trait EffectJournal: Send + Sync {
    /// Returns `send=true` only for a new step or the first claim after an operator `NotExecuted` resolution (consumed in the same commit).
    fn request(&self, request: EffectRequest) -> Reply<'_, EffectPermit>;
    /// Validates tool binding, attempt, call, and args digest without writing state or granting send;
    /// returns `Conflict` on digest mismatch and leaves unconsumed operator resolutions intact.
    fn lookup(&self, request: EffectRequest, tool: ToolId) -> Reply<'_, Option<EffectStatus>>;
    fn status(&self, attempt: Attempt, call: CallId, effect: EffectId) -> Reply<'_, EffectStatus>;
    fn unresolved(&self, attempt: Attempt, call: CallId) -> Reply<'_, Vec<EffectStatus>>;
    fn record(&self, attempt: Attempt, call: CallId, status: EffectStatus) -> Reply<'_, ()>;
}

pub struct EffectPermit {
    pub status: EffectStatus,
    pub send: bool,
}

pub trait AuditLog: Send + Sync {
    /// Appends standalone audit rows (such as denials); state transitions write their own audit entries in-transaction.
    fn append(&self, entry: AuditEntry) -> Reply<'_, u64>;
    /// Verifies the full audit chain on the trusted host; user-facing entrypoints must enforce operator authorization.
    fn verify(&self) -> Reply<'_, ChainReport>;
}
