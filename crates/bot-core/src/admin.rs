//! Admin read and operator action contracts implemented by the store and consumed by the admin API.

pub use crate::state::EffectResolution;
use crate::{id::*, port::Reply, state::*};
use std::time::SystemTime;

/// Authenticated caller; `operator` grants host-wide scope, otherwise scoped to own runs and assigned approvals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Viewer {
    pub principal: PrincipalId,
    pub operator: bool,
}

/// Pagination parameters: callers clamp `limit` to `1..=200`; `after` is a store-defined opaque cursor.
#[derive(Clone, Debug, Default)]
pub struct Page {
    pub limit: u32,
    pub after: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Listing<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct RunQuery {
    pub profile: Option<ProfileId>,
    pub state: Option<RunState>,
    pub root: Option<RunId>,
    pub page: Page,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApprovalFilter {
    Pending,
    /// Any non-pending state, including expired and superseded.
    Decided,
    #[default]
    All,
}

#[derive(Clone, Debug, Default)]
pub struct ApprovalQuery {
    pub profile: Option<ProfileId>,
    pub filter: ApprovalFilter,
    pub page: Page,
}

#[derive(Clone, Debug, Default)]
pub struct AuditQuery {
    pub profile: Option<ProfileId>,
    /// Exclusive upper sequence bound; results are returned newest-first.
    pub before: Option<u64>,
    /// API page max is 200; the store accepts up to 201 (`limit + 1`) to detect a next page.
    pub limit: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Waiting {
    Approval,
    Child(RunId),
    Hold(CallId),
}

#[derive(Clone, Debug)]
pub struct RunView {
    pub profile: ProfileId,
    pub run: RunId,
    pub root: RunId,
    pub parent: Option<RunId>,
    pub step: Option<String>,
    pub agent: AgentId,
    pub principal: PrincipalId,
    pub kind: ConversationKind,
    pub trigger: Option<String>,
    pub state: RunState,
    pub generation: Generation,
    pub attempts: u32,
    pub waiting: Option<Waiting>,
    pub private_memory_read: bool,
    /// Fixed error code from the last failure; never raw error text.
    pub error: Option<String>,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallState {
    Running,
    Awaiting,
    Done,
    Held,
    Denied,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallError {
    Denied,
    Timeout,
    InvalidResult,
    OutcomeUnknown,
    ExecutionFailed,
}

impl CallError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Timeout => "timeout",
            Self::InvalidResult => "invalid_result",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::ExecutionFailed => "execution_failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CallView {
    pub call: CallId,
    pub run: RunId,
    pub generation: Generation,
    pub tool: ToolId,
    pub tool_revision: Revision,
    pub risk: Risk,
    pub state: CallState,
    pub error: Option<CallError>,
    pub args_sha256: [u8; 32],
    pub started_at: SystemTime,
    pub finished_at: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct EffectView {
    pub effect: EffectId,
    /// Active hold occurrence for this step; re-holding allocates a new ID.
    pub hold_id: Option<HoldId>,
    pub run: RunId,
    pub call: CallId,
    pub step: String,
    pub state: EffectState,
    /// Present after operator confirms non-execution until this step restarts.
    pub resolution: Option<EffectResolution>,
    /// Downstream receipt for a confirmed step; preserved while held.
    pub receipt: Option<String>,
    pub reason: Option<String>,
    pub args_sha256: [u8; 32],
    pub generation: Generation,
    pub at: SystemTime,
}

/// Run lineage, children, calls, and effects read from a single consistent snapshot.
#[derive(Clone, Debug)]
pub struct RunDetail {
    pub run: RunView,
    /// Operator-only control preconditions for normal root runs.
    pub control: Option<RunControlView>,
    /// Ancestor chain from root through this run, inclusive.
    pub lineage: Vec<RunView>,
    pub children: Vec<RunView>,
    pub calls: Vec<CallView>,
    pub effects: Vec<EffectView>,
}

#[derive(Clone, Debug)]
pub struct RunControlView {
    /// Increments even on re-claim within the same generation.
    pub claim_epoch: ClaimEpoch,
    pub cancel_requested: bool,
    /// True if any unresolved hold exists on the root or its descendants.
    pub held: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunAction {
    Resume,
    Cancel,
}

#[derive(Clone, Debug)]
pub struct RunCommand {
    pub profile: ProfileId,
    pub run: RunId,
    pub id: String,
    pub action: RunAction,
    pub state: RunState,
    pub generation: Generation,
    pub claim_epoch: ClaimEpoch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunCommandOutcome {
    Applied,
    Held,
}

/// Source of the hold; determines post-verdict recovery (tool resume, reply retry, or private DM close).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldKind {
    Tool,
    Reply,
    Private,
}

/// Held call: `steps` is empty for single-write holds, or ordered steps (with original receipts) for multi-step tools.
#[derive(Clone, Debug)]
pub struct HoldView {
    pub kind: HoldKind,
    /// Call-level hold ID; populated when steps are settled and only the final outcome is unknown.
    pub hold_id: Option<HoldId>,
    pub profile: ProfileId,
    pub run: RunId,
    pub call: CallId,
    pub tool: ToolId,
    pub reason: String,
    pub since: SystemTime,
    pub steps: Vec<EffectView>,
}

pub use crate::ledger::ApprovalState;

#[derive(Clone, Debug)]
pub struct ApprovalView {
    pub nonce: ApprovalId,
    pub profile: ProfileId,
    pub run: RunId,
    pub generation: Generation,
    pub call: CallId,
    pub requester: PrincipalId,
    pub approver: PrincipalId,
    pub risk: Risk,
    pub tool: ToolId,
    pub tool_revision: Revision,
    pub policy_revision: Revision,
    pub args_sha256: [u8; 32],
    pub expires_at: SystemTime,
    pub state: ApprovalState,
    /// Broker-rendered `(label, value)` pairs derived from validated arguments.
    pub render: Vec<(String, String)>,
    pub decided_by: Option<PrincipalId>,
    pub decided_at: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub seq: u64,
    pub at: SystemTime,
    pub prev: [u8; 32],
    pub hash: [u8; 32],
    pub profile: Option<ProfileId>,
    pub actor: PrincipalId,
    pub action: String,
    pub target: String,
    pub args_sha256: Option<[u8; 32]>,
    pub verdict: String,
}

/// Full hash-chain verification result; `broken_at` is the first mismatched sequence number.
#[derive(Clone, Debug)]
pub struct AuditCheck {
    pub entries: u64,
    pub head: [u8; 32],
    pub broken_at: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ProfileStatus {
    pub profile: ProfileId,
    pub runs: Vec<(RunState, u64)>,
    pub oldest_ready_secs: Option<u64>,
    pub approvals_pending: u64,
    pub holds: u64,
}

/// Read-only admin queries scoped to `viewer`; never returns raw args, bodies, or secrets.
pub trait AdminRead: Send + Sync {
    fn runs(&self, viewer: &Viewer, query: RunQuery) -> Reply<'_, Listing<RunView>>;
    fn detail(&self, viewer: &Viewer, profile: ProfileId, run: RunId) -> Reply<'_, RunDetail>;
    /// Paginated after viewer scoping and filtering. `Pending` sorts by `expires_at` ascending;
    /// `Decided`/`All` sort by `decided_at` descending with undated rows last (`Pending` last among them).
    /// Ties break by `nonce` ascending; cursors preserve the active filter and sort key.
    fn approvals(&self, viewer: &Viewer, query: ApprovalQuery) -> Reply<'_, Listing<ApprovalView>>;
    fn approval(&self, viewer: &Viewer, nonce: ApprovalId) -> Reply<'_, ApprovalView>;
    /// Operator-only.
    fn holds(&self, viewer: &Viewer, profile: Option<ProfileId>) -> Reply<'_, Vec<HoldView>>;
    /// Operator-only.
    fn audit(&self, viewer: &Viewer, query: AuditQuery) -> Reply<'_, Vec<AuditEntry>>;
    /// Operator-only.
    fn verify_audit(&self, viewer: &Viewer) -> Reply<'_, AuditCheck>;
    /// Operator-only.
    fn status(&self, viewer: &Viewer) -> Reply<'_, Vec<ProfileStatus>>;
    /// Operator-only. Reads active pre-approvals, pause state, and today's UTC usage in one snapshot.
    fn preapprovals(&self, viewer: &Viewer) -> Reply<'_, Vec<crate::grant::PreapprovalRow>>;
}

/// Call-level target when `effect` is `None`.
#[derive(Clone, Debug)]
pub struct HoldRef {
    pub hold_id: HoldId,
    pub profile: ProfileId,
    pub run: RunId,
    pub call: CallId,
    pub effect: Option<EffectId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HoldVerdict {
    /// Downstream receipt proves the action already executed.
    Confirmed { receipt: String },
    /// Evidence confirms prior process exit and non-execution; safe to resume the same call ID.
    NotExecuted { evidence: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldDecision {
    Applied,
    Repeated,
}

/// Operator mutations committing state changes and audit entries atomically (approvals use `Broker::decide`).
pub trait AdminAct: Send + Sync {
    /// Applies a command against the operator-observed state, generation, and claim epoch; retries of a succeeded command return its prior outcome.
    fn control_run(&self, actor: PrincipalId, command: RunCommand) -> Reply<'_, RunCommandOutcome>;

    /// Matches hold ID and target, committing verdict and audit once. Same-actor retries with
    /// identical verdict and evidence return `Repeated` even after a new hold; mismatches return `Conflict`.
    fn resolve_hold(
        &self,
        actor: PrincipalId,
        hold: HoldRef,
        verdict: HoldVerdict,
    ) -> Reply<'_, HoldDecision>;

    /// Pauses or resumes a trigger's pre-approval using CAS on grant ID and pause generation,
    /// committing state and audit atomically. Idempotent repeats return `repeated` with no audit entry;
    /// mismatches return `Stale` and missing targets return `Missing`.
    fn set_pause(
        &self,
        actor: PrincipalId,
        key: crate::grant::TriggerKey,
        cas: crate::grant::GrantCas,
        pause: bool,
    ) -> Reply<'_, crate::grant::PauseOutcome>;
}
