//! Pre-approval grants for static cron triggers and per-event human approval evidence.
//!
//! - `Preapproval` applies only to model-free cron triggers with fixed tool and args; grants are bound to
//!   `GrantSpec::fingerprint` and revoked permanently if the declaration changes or disappears.
//! - `HumanApproval` satisfies one approval step only when the verified actor of the current event is a
//!   designated approver. Neither authority is inherited by models, delegations, or PASS candidates.
//! - Secret values never appear here; targets name exact IDs or secret names only.

use crate::id::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Fixed audit and admin reason codes; never expands public MCP or CLI refusal vocabularies.
pub mod reason {
    pub const INVALID: &str = "invalid_preapproval";
    /// Tool cannot guarantee pinned revision, target binding, or write mediation.
    pub const UNENFORCEABLE: &str = "preapproval_unenforceable";
    /// Grant revoked/replaced, pause epoch drifted, or fingerprint no longer matches.
    pub const STALE: &str = "preapproval_stale";
    pub const PAUSED: &str = "trigger_paused";
    /// Effect request falls outside allowed pairs or violates `{operation,target,params}`.
    pub const TARGET_DENIED: &str = "preapproval_target_denied";
    /// Daily UTC quota exhausted.
    pub const LIMIT: &str = "preapproval_limit";
    pub const HUMAN_MISMATCH: &str = "human_approval_mismatch";
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct GrantId(pub String);

pub const MAX_PER_DAY: u32 = 65_535;
pub const MAX_PAIRS: usize = 64;
pub const MAX_PIECE_BYTES: usize = 128;

/// Static trigger `preapproved` spec: daily write limit and sorted, deduplicated `(operation, target)` pairs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Preapproval {
    pub max_per_day: u32,
    pub targets: Vec<(String, String)>,
}

/// Sorted, deduplicated human principals permitted to approve directly via event evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanApproval {
    pub approvers: Vec<PrincipalId>,
}

/// Accepts exact IDs or secret names; rejects blanks, whitespace, wildcards, URLs, and path traversal.
pub fn piece_ok(piece: &str) -> bool {
    !piece.is_empty()
        && piece.len() <= MAX_PIECE_BYTES
        && piece.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '#' | '@' | '+')
        })
        && !piece.contains("://")
        && !piece.contains("..")
}

/// Non-secret inputs hashed into `GrantSpec::fingerprint`.
pub struct GrantInputs<'a> {
    pub profile: &'a ProfileId,
    pub agent: &'a AgentId,
    pub trigger: &'a str,
    /// Normalized trigger source (cron expression, timezone, or beat).
    pub source: &'a str,
    pub principal: &'a PrincipalId,
    /// Execution scopes resolved for the trigger principal at activation.
    pub scopes: &'a BTreeSet<String>,
    pub tool: &'a ToolId,
    /// Tool revision including the installed artifact digest.
    pub tool_revision: &'a Revision,
    /// JCS SHA-256 of fixed tool arguments.
    pub args_sha256: [u8; 32],
    pub preapproval: &'a Preapproval,
    /// Digest of tool target mapping (secret names, optional env, egress, destination).
    pub mapping: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantSpec {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub trigger: String,
    pub tool: ToolId,
    pub max_per_day: u32,
    pub pairs: Vec<(String, String)>,
    /// Canonical JSON of fingerprint inputs shown to operators; contains no secret values.
    pub summary: String,
    pub fingerprint: [u8; 32],
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl GrantSpec {
    pub fn new(input: &GrantInputs<'_>) -> Self {
        let mut pairs = input.preapproval.targets.clone();
        pairs.sort();
        pairs.dedup();
        let material: Value = json!({
            "v": 1,
            "profile": input.profile.0,
            "agent": input.agent.0,
            "trigger": input.trigger,
            "source": input.source,
            "principal": input.principal.0,
            "scopes": input.scopes.iter().collect::<Vec<_>>(),
            "tool": input.tool.0,
            "tool_revision": input.tool_revision.0,
            "args_sha256": hex(&input.args_sha256),
            "pairs": pairs.iter().map(|(o, t)| json!([o, t])).collect::<Vec<_>>(),
            "max_per_day": input.preapproval.max_per_day,
            "mapping": hex(&input.mapping),
        });
        // serde_json serializes map keys in sorted order, making the summary deterministic.
        let summary = material.to_string();
        let fingerprint: [u8; 32] = Sha256::digest(summary.as_bytes()).into();
        Self {
            profile: input.profile.clone(),
            agent: input.agent.clone(),
            trigger: input.trigger.to_owned(),
            tool: input.tool.clone(),
            max_per_day: input.preapproval.max_per_day,
            pairs,
            summary,
            fingerprint,
        }
    }

    pub fn scope_line(&self) -> String {
        let pairs: Vec<String> = self
            .pairs
            .iter()
            .map(|(operation, target)| format!("{operation}→{target}"))
            .collect();
        format!(
            "{}/{}/{}: tool {} · max {}/day · pairs {}",
            self.profile.0,
            self.agent.0,
            self.trigger,
            self.tool.0,
            self.max_per_day,
            pairs.join(", ")
        )
    }
}

/// Authority evaluated by the broker and re-checked by the store under the same call-start lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Authority {
    /// Recomputed fingerprint; the store reads grant ID and pause epoch bound to the run at admission.
    Grant { fingerprint: [u8; 32] },
    /// Approver principal matched against the event evidence bound to the run at admission.
    Human { actor: PrincipalId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Granted {
    Started,
    /// No state change; call closes as refused with this fixed reason code.
    Refused(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceKind {
    Message,
    ThreadReply,
    Reaction,
}

impl EvidenceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::ThreadReply => "thread_reply",
            Self::Reaction => "reaction",
        }
    }

    pub fn from_recorded(value: &str) -> Option<Self> {
        match value {
            "message" => Some(Self::Message),
            "thread_reply" => Some(Self::ThreadReply),
            "reaction" => Some(Self::Reaction),
            _ => None,
        }
    }
}

/// Admission-time Slack event evidence (contains no message bodies).
/// Consumed at most once per `(team, event_id)` and never inherited across `PASS`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub team: String,
    pub event_id: String,
    pub binding: BindingId,
    pub channel: String,
    /// Message `ts`, or reacted target item `ts` for reactions.
    pub item_ts: String,
    pub thread_ts: Option<String>,
    pub actor: PrincipalId,
    /// First candidate trigger (`PassChain.candidates[0]`); `PASS` never transfers evidence to later candidates.
    pub trigger: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreapprovalRow {
    pub profile: String,
    pub agent: String,
    pub trigger: String,
    pub grant_id: String,
    pub epoch: i64,
    pub paused: bool,
    pub used_today: i64,
    pub max_per_day: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerKey {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub trigger: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantCas {
    pub grant_id: String,
    pub epoch: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseOutcome {
    pub row: PreapprovalRow,
    /// True when no state change or audit entry occurred.
    pub repeated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct GrantPlan {
    /// Prepared `(profile, revision)` set matching `ActiveRevisions::replace_prepared`.
    pub prepared: std::collections::BTreeSet<(ProfileId, Revision)>,
    /// One entry per pre-approved trigger in the new set; empty revokes all grants.
    pub grants: Vec<GrantSpec>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PauseState {
    pub epoch: i64,
    pub paused: bool,
    /// Prior transition `(from_epoch, paused, grant_id)` used to recognize lost-response replays.
    pub last: Option<(i64, bool, String)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PauseDecision {
    Apply,
    Repeated,
    Stale,
}

/// Evaluates a pause/resume CAS command against current state:
/// - Mismatched `grant_id` or negative epoch -> `Stale`.
/// - Matching epoch -> `Repeated` if already in target state, else `Apply`.
/// - `epoch + 1 == state.epoch` matching `state.last` -> `Repeated` (lost-response replay); ABA and all other epochs -> `Stale`.
pub fn decide_pause(
    current_grant: &str,
    state: &PauseState,
    cas: &GrantCas,
    pause: bool,
) -> PauseDecision {
    if current_grant != cas.grant_id || cas.epoch < 0 {
        return PauseDecision::Stale;
    }
    if cas.epoch == state.epoch {
        return if state.paused == pause {
            PauseDecision::Repeated
        } else {
            PauseDecision::Apply
        };
    }
    let replay = cas.epoch.checked_add(1) == Some(state.epoch)
        && state.last.as_ref().is_some_and(|(from, kind, grant)| {
            *from == cas.epoch && *kind == pause && *grant == cas.grant_id
        });
    if replay {
        PauseDecision::Repeated
    } else {
        PauseDecision::Stale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(
        pre: &'a Preapproval,
        scopes: &'a BTreeSet<String>,
        profile: &'a ProfileId,
        agent: &'a AgentId,
        principal: &'a PrincipalId,
        tool: &'a ToolId,
        revision: &'a Revision,
    ) -> GrantInputs<'a> {
        GrantInputs {
            profile,
            agent,
            trigger: "notify",
            source: "cron:0 9 * * *@Asia/Seoul",
            principal,
            scopes,
            tool,
            tool_revision: revision,
            args_sha256: [7; 32],
            preapproval: pre,
            mapping: [9; 32],
        }
    }

    #[test]
    fn the_fingerprint_binds_every_piece_and_ignores_pair_order() {
        let (profile, agent) = (ProfileId("p".into()), AgentId("main".into()));
        let (principal, tool, revision) = (
            PrincipalId("service:reporter".into()),
            ToolId("reports.notify".into()),
            Revision("r1@img".into()),
        );
        let scopes: BTreeSet<String> = ["a".to_owned()].into();
        let pre = Preapproval {
            max_per_day: 24,
            targets: vec![
                ("post".into(), "channel:reports".into()),
                ("post".into(), "channel:alerts".into()),
            ],
        };
        let base = GrantSpec::new(&inputs(
            &pre, &scopes, &profile, &agent, &principal, &tool, &revision,
        ));
        let reordered = Preapproval {
            max_per_day: 24,
            targets: pre.targets.iter().rev().cloned().collect(),
        };
        assert_eq!(
            base.fingerprint,
            GrantSpec::new(&inputs(
                &reordered, &scopes, &profile, &agent, &principal, &tool, &revision
            ))
            .fingerprint,
            "pair order must not change the fingerprint"
        );
        let mut changed = pre.clone();
        changed.max_per_day = 25;
        assert_ne!(
            base.fingerprint,
            GrantSpec::new(&inputs(
                &changed, &scopes, &profile, &agent, &principal, &tool, &revision
            ))
            .fingerprint
        );
        let other = Revision("r1@img2".into());
        assert_ne!(
            base.fingerprint,
            GrantSpec::new(&inputs(
                &pre, &scopes, &profile, &agent, &principal, &tool, &other
            ))
            .fingerprint,
            "tool revision change must change the fingerprint"
        );
        assert!(base.scope_line().contains("post→channel:alerts"));
    }

    fn cas(epoch: i64) -> GrantCas {
        GrantCas {
            grant_id: "g1".into(),
            epoch,
        }
    }

    #[test]
    fn pause_commands_apply_once_and_replays_are_exact() {
        let fresh = PauseState::default();
        assert_eq!(
            decide_pause("g1", &fresh, &cas(0), true),
            PauseDecision::Apply
        );
        assert_eq!(
            decide_pause("g1", &fresh, &cas(0), false),
            PauseDecision::Repeated,
            "already unpaused"
        );
        let paused = PauseState {
            epoch: 1,
            paused: true,
            last: Some((0, true, "g1".into())),
        };
        assert_eq!(
            decide_pause("g1", &paused, &cas(1), true),
            PauseDecision::Repeated,
            "same epoch and already paused"
        );
        assert_eq!(
            decide_pause("g1", &paused, &cas(0), true),
            PauseDecision::Repeated,
            "replay after lost response"
        );
        assert_eq!(
            decide_pause("g1", &paused, &cas(0), false),
            PauseDecision::Stale,
            "different command kind"
        );
        assert_eq!(
            decide_pause("g1", &paused, &cas(1), false),
            PauseDecision::Apply
        );
        // Replaying the first pause after pause -> resume -> pause is stale even though paused matches.
        let again = PauseState {
            epoch: 3,
            paused: true,
            last: Some((2, true, "g1".into())),
        };
        assert_eq!(
            decide_pause("g1", &again, &cas(0), true),
            PauseDecision::Stale
        );
        assert_eq!(
            decide_pause("g1", &again, &cas(2), true),
            PauseDecision::Repeated
        );
        assert_eq!(
            decide_pause("g2", &again, &cas(3), true),
            PauseDecision::Stale,
            "grant was replaced"
        );
        let max = PauseState {
            epoch: i64::MAX,
            paused: true,
            last: None,
        };
        assert_eq!(
            decide_pause("g1", &max, &cas(i64::MAX), true),
            PauseDecision::Repeated
        );
        assert_eq!(
            decide_pause("g1", &max, &cas(-1), true),
            PauseDecision::Stale
        );
    }

    #[test]
    fn pieces_reject_wildcards_urls_and_blanks() {
        for good in ["post", "channel:reports", "mail-bot", "user@team", "a.b_c"] {
            assert!(piece_ok(good), "{good}");
        }
        for bad in [
            "",
            "*",
            "chan*",
            "https://hooks.example/x",
            "a b",
            "a/b",
            "a..b",
            "line\nbreak",
            "?x",
        ] {
            assert!(!piece_ok(bad), "{bad:?}");
        }
    }
}
