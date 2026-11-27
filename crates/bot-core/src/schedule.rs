//! User-created chat schedules, due-queue pagination, and explicit-destination delivery ports.
use crate::{
    command::JsonPayload,
    cron::Cron,
    id::{
        AgentId, Attempt, BindingId, CallId, PrincipalId, ProfileId, Revision, RunId, ScheduleId,
    },
    ledger::StartCall,
    port::{Error, Reply},
    profile::Scopes,
    state::Conversation,
};
use std::time::SystemTime;

mod close;
mod create;
mod delivery;
mod unsent;
pub mod utc;
pub use close::CloseReason;
pub use create::{CreateSeal, valid_iana_timezone};
pub use delivery::{DELIVER_CALL, DELIVER_REVISION, DELIVER_TOOL, DELIVERY_CARD_LABEL};
pub use unsent::{DeliveryFence, UNROUTED_NOTICE, UnsentReason};

/// Applies across all agents of the same `(profile, owner)`.
pub const MAX_ACTIVE_PER_OWNER: usize = 32;
pub const MAX_INSTRUCTION_BYTES: usize = 8192;
/// Caps chained automatic wake-ups to prevent unbounded self-scheduling.
pub const MAX_WAKE_DEPTH: u8 = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Timing {
    At(SystemTime),
    Cron { expression: Cron, timezone: String },
}

/// Even with an explicit destination, the origin conversation governs memory visibility and approval routing.
#[derive(Clone)]
pub struct DeliveryTarget {
    pub binding: BindingId,
    pub conversation: Conversation,
}

/// Validates explicit destinations against the operator's host registry; CLI/HTTP origins only reply in place.
pub trait ScheduleRoutes: Send + Sync {
    /// Pins a destination ID to its canonical delivery route while preserving binding, kind, and thread (no external lookups).
    /// Slack channel IDs normalize to `team/channel`. Checked at create, fire, and send; unmapped or rebound routes return `Denied`.
    fn resolve(
        &self,
        profile: &ProfileId,
        target: &DeliveryTarget,
    ) -> Result<DeliveryTarget, Error>;
}

#[derive(Clone)]
pub struct ScheduleSpec {
    pub timing: Timing,
    pub instruction: String,
    pub quiet: bool,
    /// `None` replies to the origin conversation; `Some` targets an explicitly allowed route in the same profile.
    pub destination: Option<DeliveryTarget>,
}

/// Intentionally omits instruction text, destination, and origin conversation details.
#[derive(Clone, Debug)]
pub struct ScheduleSummary {
    pub id: ScheduleId,
    pub timing: Timing,
    pub quiet: bool,
}

/// Includes the prior cursor and due timestamp so competing schedulers converge on the same occurrence.
#[derive(Clone)]
pub struct DueSchedule {
    pub profile: ProfileId,
    pub id: ScheduleId,
    pub owner: PrincipalId,
    pub agent: AgentId,
    pub origin: DeliveryTarget,
    pub ceiling: Scopes,
    pub spec: ScheduleSpec,
    pub cursor: SystemTime,
    /// Leading key of the `(at, profile, id)` scan order, used to interleave with `UnreadableSchedule`.
    pub at: SystemTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuePos {
    pub at: SystemTime,
    pub profile: ProfileId,
    pub id: ScheduleId,
}

/// Pagination cursor over `after < pos <= end`, where `end` is frozen at sweep start so newly due rows cannot starve skipped ones.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DueKey {
    pub after: DuePos,
    pub end: DuePos,
}

/// Corrupt schedule row returned separately so one bad row cannot block a page; never fired, closed with `reason`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnreadableSchedule {
    pub profile: ProfileId,
    pub id: ScheduleId,
    pub cursor: SystemTime,
    pub at: SystemTime,
    /// `InvalidTiming` or `InvalidDestination`.
    pub reason: CloseReason,
}

pub struct DuePage {
    pub now: SystemTime,
    pub schedules: Vec<DueSchedule>,
    /// Corrupt rows excluded from firing; `due` is read-only so the scheduler closes each row individually.
    pub unreadable: Vec<UnreadableSchedule>,
    /// Continuation key for this sweep, or `None` once `end` is reached.
    pub next: Option<DueKey>,
}

pub enum NextOccurrence {
    /// Enqueues one run and advances the next due check atomically; `next: None` marks the final occurrence.
    Fire {
        at: SystemTime,
        next: Option<SystemTime>,
        scopes: Scopes,
    },
    /// Future first cron occurrence; avoids advancing the cursor past an unfired slot.
    WaitUntil(SystemTime),
    /// Atomically closes the schedule, records audit, and posts an origin notice when applicable.
    Close(CloseReason),
}

pub struct AdvanceSchedule {
    pub profile: ProfileId,
    pub id: ScheduleId,
    pub expected_cursor: SystemTime,
    /// `None` is valid only for `Close(ProfileInactive)`, verified by the store.
    pub checked_revision: Option<Revision>,
    pub next: NextOccurrence,
}

/// One-shot follow-up requested by a trusted tool response; always replies to the origin conversation.
pub struct Wake {
    pub at: SystemTime,
    pub instruction: String,
    pub quiet: bool,
}

pub trait UserSchedules: Send + Sync {
    /// Validates auth, args, approval, and owner quota, committing creation, call completion, and audit together.
    fn create(&self, start: StartCall, spec: ScheduleSpec) -> Reply<'_, ScheduleId>;
    /// Checks claim and active revision, returning only the caller's active schedules ordered by ID.
    fn list(&self, attempt: Attempt, checked: Revision) -> Reply<'_, Vec<ScheduleSummary>>;
    /// Atomically cancels the owner's schedule, completes the call, and audits; already-accepted runs continue independently.
    fn cancel(&self, start: StartCall, id: ScheduleId) -> Reply<'_, ()>;
    /// Atomically persists a `Started` tool result and its unique follow-up schedule for `(run, call)`.
    fn finish_with_wake(
        &self,
        attempt: Attempt,
        call: CallId,
        checked: Revision,
        result: JsonPayload,
        wake: Wake,
    ) -> Reply<'_, ScheduleId>;
    /// Scans schedules due by DB time in `(at, profile, id)` order up to `limit`, using `DueKey` sweeps so skipped rows do not starve later ones.
    fn due(&self, after: Option<DueKey>, limit: u32) -> Reply<'_, DuePage>;
    /// Verifies active revision, schedule state, and cursor under lock; deduplicates each occurrence to one run.
    fn advance(&self, command: AdvanceSchedule) -> Reply<'_, Option<RunId>>;
}

/// Applies only to successful scheduled final text; approvals, errors, and attachments follow their normal paths.
pub fn quiet_completion(quiet: bool, final_text: &str) -> bool {
    quiet && final_text.trim() == "<quiet>"
}

/// Frozen reply bundle awaiting approval before external delivery; intentionally omits `Debug`.
pub struct DeliveryDraft {
    /// Host active set revision pinning gateway recipient mappings.
    pub activation: Revision,
    pub target: DeliveryTarget,
    /// Ordered final gateway payloads; approvers must review any attachments.
    pub answers: Vec<JsonPayload>,
}

/// Shared PG port for workers and the decision broker; drafts and approvals are visible only to the origin owner.
pub trait ScheduledReplies: Send + Sync {
    /// Returns the settled queued/unsent receipt for the latest claim (`None` if unsettled) without exposing draft body or destination.
    fn receipt(&self, attempt: Attempt) -> Reply<'_, Option<JsonPayload>>;

    /// `Some` freezes the final result once; `None` reads an existing draft on resume.
    /// Returns `None` for normal, origin-bound, or quiet attachment-free runs.
    fn draft(
        &self,
        attempt: Attempt,
        completion: Option<crate::command::Finish>,
    ) -> Reply<'_, Option<DeliveryDraft>>;
    /// Reads only the pinned destination under the current set and valid claim to detect revocation without exposing draft content.
    fn target(&self, attempt: Attempt) -> Reply<'_, Option<DeliveryTarget>>;
    /// Settles one occurrence after verifying set, fence, and stored target; upgrades `Unrouted` to `Rejected` if owner rejection/revocation is already recorded.
    /// `Unrouted` atomically posts a fixed origin notice, invalidates pending approvals, records the unsent receipt, and finishes the run; redelivery returns the committed receipt.
    fn finish_unsent(
        &self,
        fence: DeliveryFence,
        target: DeliveryTarget,
        reason: UnsentReason,
    ) -> Reply<'_, JsonPayload>;
    /// Verifies current human owner, permissions, active set, and draft `sha256()`, then consumes `DELIVER_CALL` approval, enqueues destination outbox rows, and finishes the run atomically.
    fn enqueue(&self, approval: StartCall) -> Reply<'_, ()>;
}

/// Host entry point for releasing scheduled replies; `Unavailable` retries without sending, notifying, or consuming approval.
pub trait ScheduleDelivery: Send + Sync {
    fn release(&self, attempt: Attempt) -> Reply<'_, crate::command::CallResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_quiet_success_suppresses_the_scheduled_reply() {
        assert!(quiet_completion(true, " <quiet>\n"));
        for text in ["", "no changes", "check needed <quiet>"] {
            assert!(!quiet_completion(true, text));
        }
        assert!(!quiet_completion(false, "<quiet>"));
    }
}
