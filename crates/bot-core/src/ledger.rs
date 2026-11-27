//! Call and private-DM ledger ports where the store commits generation checks, transitions, and audits atomically.

use crate::{command::JsonPayload, id::*, port::Reply, state::*};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalState {
    Pending,
    Approved,
    Consumed,
    Denied,
    Expired,
    Superseded,
    Revoked,
}

#[derive(Clone, Debug)]
pub enum Resolution {
    NotExecuted,
    Executed { receipt: String },
}

#[derive(Clone, Debug)]
pub enum CallPhase {
    Open {
        approval: Option<ApprovalId>,
    },
    Started {
        generation: Generation,
        owner: String,
        /// At least one downstream step is confirmed; a lost final result cannot be treated as unexecuted.
        confirmed_step: bool,
    },
    Done(JsonPayload),
    Denied,
    Held {
        reason: String,
        resolution: Option<Resolution>,
    },
}

#[derive(Clone, Debug)]
pub struct CallRecord {
    pub call: CallId,
    pub tool: ToolId,
    pub args_sha256: [u8; 32],
    pub phase: CallPhase,
}

#[derive(Clone, Debug)]
pub struct StoredApproval {
    pub approval: Approval,
    pub reasons: Vec<String>,
    pub card: JsonPayload,
    pub state: ApprovalState,
    /// Post receipt when delivered via owner DM; `None` before confirmation or for non-DM cards.
    pub delivery: Option<crate::cards::CardReceipt>,
}

pub struct OpenApproval {
    pub approval: Approval,
    pub reasons: Vec<String>,
    pub card: JsonPayload,
    /// Slack channel binding (same profile and workspace) used to route OpenAI-origin approval cards to an approver's DM.
    /// Set by the broker only when pinned and active revisions agree; `None` uses the default route.
    pub delivery: Option<BindingId>,
}

pub struct Decision {
    pub nonce: ApprovalId,
    pub actor: PrincipalId,
    pub allow: bool,
}

pub struct StartCall {
    /// Active profile revision used for the final authorization check.
    pub checked_revision: Revision,
    pub risk: Risk,
    pub tool_revision: Revision,
    pub attempt: Attempt,
    pub call: CallId,
    pub approval: Option<(Approval, Vec<String>)>,
    pub private_recall: bool,
    pub owner: String,
}

pub enum Outcome {
    Done(JsonPayload),
    Released,
    Held(String),
    Blocked,
}

pub struct FinishCall {
    pub attempt: Attempt,
    pub call: CallId,
    pub outcome: Outcome,
}

/// Store derives audit actor, target, and verdict from authenticated requests and state transitions.
pub trait CallLedger: Send + Sync {
    /// Actor-bound, read-only policy context while parked. Returned attempt has no live claim.
    fn approval_context(&self, nonce: ApprovalId, actor: PrincipalId) -> Reply<'_, RunContext>;
    fn context(&self, attempt: Attempt) -> Reply<'_, RunContext>;
    /// Reads an existing call after validating claim, generation, lease, and cancellation; read-only (no creation, consumption, or audit).
    fn lookup(&self, attempt: Attempt, call: CallId) -> Reply<'_, Option<CallRecord>>;
    /// Reads a checkpoint under a valid normal claim (`Stale` if handed over/expired/cancelled, `Denied` for private DM).
    fn step(&self, attempt: Attempt, key: String) -> Reply<'_, Option<JsonPayload>>;
    /// Reads the profile's active revision under a valid normal claim (`None` if inactive, `Stale` if invalid, `Denied` for private DM).
    fn active_revision(&self, attempt: Attempt) -> Reply<'_, Option<Revision>>;
    fn begin(
        &self,
        attempt: Attempt,
        call: CallId,
        tool: ToolId,
        args_sha256: [u8; 32],
    ) -> Reply<'_, CallRecord>;
    fn open_approval(&self, open: OpenApproval) -> Reply<'_, ()>;
    fn approval(&self, nonce: ApprovalId) -> Reply<'_, StoredApproval>;
    /// Conditionally closes the active call's approval, waking and auditing atomically; idempotent and never alters consumed approvals.
    fn supersede(&self, nonce: ApprovalId, actor: PrincipalId) -> Reply<'_, ()>;
    fn decide(&self, decision: Decision) -> Reply<'_, ApprovalState>;
    /// Serializes with active-revision swaps; returns `Stale` before consuming approval on mismatch or inactivity.
    /// Atomically verifies generation, cancellation, private flag, and approval, then records start.
    /// Rejects `memory.share` (use `MemoryStore::share` to commit approval and sharing together).
    fn start(&self, start: StartCall) -> Reply<'_, ()>;
    /// Like `start`, but also verifies pre-approval or designated-approver evidence in the same commit.
    /// Returns `Granted::Refused(code)` without mutating on failure; direct calls on pre-approved runs cannot bypass via `start`.
    fn start_with(
        &self,
        start: StartCall,
        authority: crate::grant::Authority,
    ) -> Reply<'_, crate::grant::Granted>;
    /// Read-only intake-bound evidence for root runs (`None` if absent).
    fn evidence(&self, attempt: Attempt) -> Reply<'_, Option<crate::grant::Evidence>>;
    fn finish(&self, finish: FinishCall) -> Reply<'_, ()>;
    /// Closes a call as `Outcome::Blocked` with its `ApprovalBlock` reason for `ApiRuns::lookup`.
    fn finish_blocked(&self, finish: FinishCall, why: crate::api::ApprovalBlock) -> Reply<'_, ()>;
    /// Records a direct-tool `PASS` (`finish.outcome` must be stripped `Done`), advancing the cursor and auditing atomically before re-queuing.
    /// Returns `PassNext::Refused` or `Stale` without mutating if effects, holds, approvals, or replies exist or `pos` mismatches.
    /// `runnable` holds agents the requester may `run` under both active and pinned revisions; default conversation starts only if included and allowed by intake scopes, else the run ends silently.
    fn finish_pass(
        &self,
        finish: FinishCall,
        pos: u32,
        runnable: std::collections::BTreeSet<AgentId>,
    ) -> Reply<'_, crate::pass::PassNext>;
}

/// Private DM body stays in memory only; the ledger stores only HMACs and the request key.
pub struct PrivateRequest {
    pub key: crate::intake::RequestKey,
    /// Direct-handling HMAC; legacy rows match this format.
    pub fingerprint: [u8; 32],
    /// v2 HMAC binding revision, trigger, and full envelope; present only when the matched declaration allows pass.
    pub pass_fingerprint: Option<[u8; 32]>,
    pub revision: Revision,
    pub agent: AgentId,
    pub tool: ToolId,
    pub conversation: Conversation,
    pub scopes: crate::profile::Scopes,
}

/// Pre-dispatch revalidation inputs; carries no sensitive payload.
pub struct PrivateStart {
    pub attempt: Attempt,
    pub tool: ToolId,
    pub fingerprint: [u8; 32],
    pub checked_revision: Revision,
}

/// Intake-only ledger inaccessible to normal workers; call ID stays fixed across re-claims of the same intake.
pub trait PrivateLedger: Send + Sync {
    /// Validates new intakes against the active revision; `Start` grants a reserved intake lease.
    fn begin(&self, request: PrivateRequest) -> Reply<'_, PrivatePermit>;
    /// Verifies lease, epoch, tool, HMAC, and active revision, committing once immediately before dispatch.
    fn start(&self, start: PrivateStart) -> Reply<'_, ()>;
    fn finish(&self, permit: Attempt, outcome: PrivateOutcome) -> Reply<'_, ()>;
}

pub enum PrivatePermit {
    Start {
        attempt: Attempt,
        /// Pass envelope was bound on initial intake and matches this delivery.
        allow_pass: bool,
    },
    InProgress,
    Completed,
    Passed,
    Rejected,
    Held,
}

pub enum PrivateOutcome {
    /// Releases lease only if never started; re-claimable only by redelivering identical input.
    RetryBeforeStart,
    /// Runner confirmed pre-send failure and exit of the prior process.
    NotSent,
    Completed,
    Passed,
    Held,
    Rejected,
}
