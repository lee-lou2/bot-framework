//! Command and value types exchanged across store, runtime, broker, and gateway ports.

use crate::{id::*, intake::PushTransportId, state::*};
use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime},
};

/// Schema-validated UTF-8 JSON document at a trust boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonPayload(pub String);

/// Uppercase HTTP method and mount-relative path (leading `/`, no query string).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRoute {
    pub method: String,
    pub path: String,
    /// Response mode chosen at initial routing; excluded from duplicate-intake hashing.
    pub response: crate::profile::ResponseMode,
}

#[derive(Clone, Debug)]
pub struct Envelope {
    pub profile: ProfileId,
    pub request: RequestId,
    pub binding: BindingId,
    pub principal: PrincipalId,
    pub conversation: Conversation,
    /// Present only for authenticated HTTP intake; method and path join duplicate-intake detection.
    pub http: Option<HttpRoute>,
    pub attachments: Vec<String>,
    pub body: JsonPayload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Agent {
        agent: AgentId,
        instruction: String,
    },
    Tool {
        agent: AgentId,
        tool: ToolId,
        args: JsonPayload,
    },
}

/// `scopes` snapshots requester permissions at intake; the broker intersects with current policy on every call.
#[derive(Clone, Debug)]
pub struct Accept {
    /// Delivery responsibility set by authenticated push ingress; `None` for pull ingress and static schedules.
    pub reply_transport: Option<PushTransportId>,
    pub event: Envelope,
    pub approver: Option<PrincipalId>,
    pub revision: Revision,
    pub target: Target,
    pub scopes: BTreeSet<String>,
}

/// Populated only by authenticated adapters (never from payloads, models, or tools).
#[derive(Clone, Debug, Default)]
pub struct AcceptExtras {
    pub source: Option<crate::slack::SlackSource>,
    /// Direct-tool pass chain; the first candidate is `Accept.target`.
    pub chain: Option<crate::pass::PassChain>,
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub attempt: Attempt,
    pub call: CallId,
    pub tool: ToolId,
    pub args: JsonPayload,
}

#[derive(Clone, Debug)]
pub enum CallResult {
    Done(JsonPayload),
    Awaiting(ApprovalId),
    Held(CallId),
    /// Direct call yielded `bot/pass`; store advanced the cursor, re-queued the run for the next candidate (or default agent), and released the lease.
    Passed,
}

#[derive(Clone, Debug)]
pub enum MemoryChange {
    Create { text: String },
    Edit { id: MemoryId, text: String },
    Delete { id: MemoryId },
}

#[derive(Clone, Debug)]
pub struct Occurrence {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub trigger: String,
    pub scheduled_at: std::time::SystemTime,
}

/// `scopes` is narrowed by runtime policy; the store accepts only subsets of the parent's scopes.
#[derive(Clone, Debug)]
pub struct Delegate {
    /// Active profile revision that authorized delegation, checked independently of the parent run's pinned revision.
    pub checked_revision: Revision,
    pub parent: Attempt,
    pub step: String,
    pub child: AgentId,
    pub instruction: String,
    pub scopes: BTreeSet<String>,
}

impl Delegate {
    /// Shared parser for both MCP calls and session resume.
    pub fn target(input: &serde_json::Value) -> Option<(AgentId, String)> {
        let object = input.as_object()?;
        let child = object.get("agent")?.as_str()?;
        let instruction = object.get("instruction")?.as_str()?.trim();
        (object.len() == 2 && !child.is_empty() && !instruction.is_empty())
            .then(|| (AgentId(child.to_owned()), instruction.to_owned()))
    }

    pub fn parse_target(args: &JsonPayload) -> Option<(AgentId, String)> {
        Self::target(&crate::canon::parse(&args.0)?)
    }
}

#[derive(Clone, Debug)]
pub struct EffectRequest {
    pub attempt: Attempt,
    pub call: CallId,
    pub step: String,
    pub args: JsonPayload,
}

#[derive(Clone, Debug)]
pub struct EffectStatus {
    pub effect: EffectId,
    pub state: EffectState,
    /// Only unconsumed operator decisions while `Held`; `NotExecuted` allows reclaiming the same step. Lookups never authorize transmission, and tool result reports pass `None`.
    pub resolution: Option<EffectResolution>,
    pub receipt: Option<String>,
}

/// Resume checkpoint; `key` is unique per run, and duplicate writes succeed only with identical content. Child delegation results also land here under their step key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub key: String,
    pub body: JsonPayload,
}

#[derive(Clone, Debug)]
pub struct Work {
    pub revision: Revision,
    pub target: Target,
    pub input: JsonPayload,
    pub attachments: Vec<String>,
    pub steps: Vec<Step>,
    /// Pass-chain cursor (`0` = first candidate, `None` = no chain); equal to chain length means default agent turn.
    pub pass_pos: Option<u32>,
}

/// Committed atomically with the run result; reply order matches `answers` indices.
#[derive(Clone, Debug)]
pub struct Finish {
    pub result: JsonPayload,
    pub answers: Vec<JsonPayload>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion {
    Succeeded,
    Cancelled,
}

/// Provider-reported token counts; `prompt` includes provider-defined cached input tokens once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenUsage {
    pub prompt: u64,
    pub completion: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunOutcome {
    /// True when output hit the host max-output-tokens ceiling.
    pub truncated: bool,
    /// Present only when reliable totals cover the entire run; `None` across resumes, delegations, or unmeasured runs (never zero-filled).
    pub usage: Option<TokenUsage>,
}

/// Fixed vocabulary reason codes only (never free-form sentences or payloads).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    Transient(&'static str),
    Permanent(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Retry {
    After(Duration),
    Dead,
}

/// Checked before the next external side effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Beat {
    pub cancel_requested: bool,
}

/// Prior conversation turn; `from: None` denotes an agent reply.
#[derive(Clone, Debug)]
pub struct Turn {
    pub from: Option<PrincipalId>,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct MemoryHit {
    pub id: MemoryId,
    pub visibility: Visibility,
    pub text: String,
}

/// Full body shown on approval cards; intentionally omits `Debug` to avoid leaking text in diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct MemoryPreview {
    pub id: MemoryId,
    pub visibility: Visibility,
    pub text: String,
    /// SHA-256 of the raw UTF-8 body without normalization or whitespace trimming.
    pub sha256: [u8; 32],
}

/// Binds the memory ID and reviewed body digest; keys follow RFC 8785 order.
pub fn memory_share_hash(id: &MemoryId, body_sha256: &[u8; 32]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let digest = sha2::digest::Output::<Sha256>::from(*body_sha256);
    let args = serde_json::json!({"id": id.0, "sha256": format!("{digest:x}")});
    Sha256::digest(args.to_string().as_bytes()).into()
}

/// Registered once on profile activation; deduplicated by `(profile, agent, path, sha256)`.
#[derive(Clone, Debug)]
pub struct MemorySeed {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub path: String,
    pub sha256: [u8; 32],
    pub owner: PrincipalId,
    pub visibility: Visibility,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggerSpec {
    pub approver: Option<PrincipalId>,
    pub trigger: String,
    pub cron: String,
    pub timezone: String,
    /// Health-check schedule; the only kind allowed to skip or coalesce occurrences.
    pub beat: bool,
    pub principal: PrincipalId,
    pub scopes: BTreeSet<String>,
    pub target: Target,
}

/// Static schedule definitions for a revision; segment boundaries use the activating transaction's DB timestamp.
#[derive(Clone, Debug)]
pub struct ScheduleSet {
    pub profile: ProfileId,
    pub revision: Revision,
    pub triggers: Vec<TriggerSpec>,
}

/// Applies over `(from, until]`; `until: None` is the current open segment.
#[derive(Clone, Debug)]
pub struct Segment {
    pub revision: Revision,
    pub from: SystemTime,
    pub until: Option<SystemTime>,
    pub spec: TriggerSpec,
}

#[derive(Clone, Debug)]
pub struct ScheduledTrigger {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub trigger: String,
    /// Occurrences at or before this timestamp have been accepted.
    pub cursor: SystemTime,
    /// Segments overlapping `> cursor`, oldest first.
    pub segments: Vec<Segment>,
}

#[derive(Clone, Debug)]
pub struct ScheduleSnapshot {
    /// Database clock; never the scheduler host clock.
    pub now: SystemTime,
    pub triggers: Vec<ScheduledTrigger>,
}

#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub profile: ProfileId,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub args_sha256: Option<[u8; 32]>,
    pub verdict: String,
}

#[derive(Clone, Debug)]
pub struct ChainReport {
    pub entries: u64,
    pub head: [u8; 32],
    /// First sequence number where the audit chain breaks, or `None` when intact.
    pub first_bad: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{MemoryId, memory_share_hash};
    use sha2::{Digest, Sha256};

    #[test]
    fn memory_share_hash_matches_the_canonical_json_vectors() {
        for (id, body, expected) in [
            (
                "m-1",
                "café line\n",
                "fa455d313c54c40e6005e3fc99b90ed36384fa17eabaefad6e30e475c1b50d40",
            ),
            (
                "m-\"\\",
                "A\r\nB",
                "e4475fd51fb76dfd8a7fb09d20875dc08ce071d49dfd6203b7166884f5e6d5c4",
            ),
        ] {
            let hash = memory_share_hash(&MemoryId(id.into()), &Sha256::digest(body).into());
            let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
            assert_eq!(hex, expected);
        }
    }
}

#[cfg(test)]
mod delegation_input_tests {
    use super::{AgentId, Delegate, JsonPayload};
    use serde_json::json;

    #[test]
    fn restored_and_live_delegations_accept_the_same_target_and_instruction() {
        let input = json!({"agent": "main/research", "instruction": "  read  "});
        let wanted = Some((AgentId("main/research".into()), "read".to_owned()));
        assert_eq!(Delegate::target(&input), wanted);
        assert_eq!(
            Delegate::parse_target(&JsonPayload(input.to_string())),
            wanted
        );
        for bad in [
            json!({"agent": "main/research", "instruction": "read", "extra": true}),
            json!({"agent": "main/research", "instruction": "   "}),
            json!({"agent": 1, "instruction": "read"}),
        ] {
            assert_eq!(Delegate::target(&bad), None);
            assert_eq!(Delegate::parse_target(&JsonPayload(bad.to_string())), None);
        }
        let duplicate = r#"{"agent":"main/a","agent":"main/b","instruction":"read"}"#;
        assert_eq!(Delegate::parse_target(&JsonPayload(duplicate.into())), None);
    }
}
