//! Intake and lookup port for keyed long-running HTTP requests (e.g., OpenAI-compatible ingress).
//!
//! - `ApiRuns::accept` reserves the idempotency key, enforces `max_active`, and admits the run in one transaction.
//!   Same `(profile, endpoint, principal, key)` replays on matching digest without using a slot, or returns `Conflict`.
//! - `ApiRuns::lookup` scopes strictly to `(profile, endpoint, principal, key)` so foreign runs are indistinguishable from absent.
//!   Admission-time model, binding, agent, and timestamp stay frozen across rebindings and revision swaps.

use crate::{
    command::{Accept, TokenUsage},
    id::{AgentId, BindingId, PrincipalId, ProfileId, RequestId, Revision, RunId},
    port::Reply,
};

#[derive(Clone, Debug)]
pub struct ApiAccept {
    /// `event.request` is the 1..=128-byte idempotency key; `event.principal` is `http:<alias>/<subject>`.
    /// Conversation and thread are server-assigned per POST and excluded from `digest`.
    pub accept: Accept,
    /// Validated host endpoint alias; must match the alias in `accept.event.principal`.
    pub endpoint: String,
    /// Public model slug (1..=63 lowercase) fixed on the binding.
    pub model: String,
    /// SHA-256 of canonical JSON over model, validated messages, and execution options.
    pub digest: [u8; 32],
    /// Endpoint active-run cap (1..=256), counting waiting runs; delegated children share the root slot.
    pub max_active: u32,
    /// Slack `team_id` resolved at admission for Slack approval routes; pins later cards to the same installation.
    pub team: Option<String>,
}

/// `Err(Error::Unavailable)` is valid only when definitely pre-commit (503-safe).
/// Return `Unknown` when commit confirmation is lost so keyed callers reconcile via `lookup`.
#[derive(Clone, Debug)]
pub enum ApiAdmission {
    Accepted(ApiRun),
    /// Matching key and digest; reuses the existing run without consuming an active slot.
    Replayed(ApiRun),
    Conflict,
    Capacity,
    Stale,
    /// Commit outcome could not be confirmed.
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiKey {
    pub profile: ProfileId,
    pub endpoint: String,
    pub principal: PrincipalId,
    pub key: RequestId,
}

/// Closed set of approval-route failures that block tool execution and prevent model success summaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalBlock {
    NoApprover,
    ApproverRevoked,
    RouteChanged,
    ApprovalDmUnavailable,
}

impl ApprovalBlock {
    pub const ALL: [ApprovalBlock; 4] = [
        Self::NoApprover,
        Self::ApproverRevoked,
        Self::RouteChanged,
        Self::ApprovalDmUnavailable,
    ];

    pub const fn code(self) -> &'static str {
        match self {
            Self::NoApprover => "no_approver",
            Self::ApproverRevoked => "approver_revoked",
            Self::RouteChanged => "route_changed",
            Self::ApprovalDmUnavailable => crate::cards::APPROVAL_DM_UNAVAILABLE,
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.code() == code)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiState {
    Active,
    /// Waiting on human approval (excludes child-wait and operator holds).
    NeedsHuman,
    Held,
    /// Final text saved by the worker (`RunStore::finish_with`); excludes raw tool output and attachments.
    Final {
        text: String,
        truncated: bool,
        /// Present only when trustworthy totals exist for the full run; never zero-filled.
        usage: Option<TokenUsage>,
    },
    /// Recorded on root or delegated child when approval could not proceed; never overrides `NeedsHuman`, `Failed`, or `Cancelled`.
    ApprovalBlocked(ApprovalBlock),
    /// Fixed vocabulary failure reason code.
    Failed {
        reason: String,
    },
    Cancelled,
}

/// Admitted run whose model, binding, agent, revision, created time, and digest stay frozen from admission.
#[derive(Clone, Debug)]
pub struct ApiRun {
    pub run: RunId,
    /// DB-clock Unix seconds at initial admission.
    pub created: u64,
    pub model: String,
    pub binding: BindingId,
    pub agent: AgentId,
    pub revision: Revision,
    /// Semantic digest saved at admission for reconciling `Unknown` outcomes.
    pub digest: [u8; 32],
    pub state: ApiState,
}

/// Ledger-verified OpenAI-compatible HTTP ingress origin (never inferred from webhooks or payload fields).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiOrigin {
    pub endpoint: String,
    pub model: String,
}

pub trait ApiRuns: Send + Sync {
    fn accept(&self, request: ApiAccept) -> Reply<'_, ApiAdmission>;
    fn lookup(&self, key: ApiKey) -> Reply<'_, Option<ApiRun>>;
}

/// External client turn role; preserved verbatim and always treated as untrusted input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnRole {
    System,
    Developer,
    User,
    Assistant,
}

impl TurnRole {
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "system" => Some(Self::System),
            "developer" => Some(Self::Developer),
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }
}

/// Validated turn; `name` is untrusted data unrelated to authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatTurn {
    pub role: TurnRole,
    pub content: String,
    pub name: Option<String>,
}

/// Untrusted conversation payload (`{"messages":[{"role","content","name"?}…]}`).
/// Only the authenticated ledger (`RunContext::api`) authorizes reading a run body as `ChatBody`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatBody {
    pub turns: Vec<ChatTurn>,
}

impl ChatBody {
    pub fn to_payload(&self) -> crate::command::JsonPayload {
        let messages: Vec<serde_json::Value> = self
            .turns
            .iter()
            .map(|t| {
                let mut m = serde_json::json!({"role": t.role.label(), "content": t.content});
                if let Some(name) = &t.name {
                    m["name"] = serde_json::Value::String(name.clone());
                }
                m
            })
            .collect();
        crate::command::JsonPayload(serde_json::json!({ "messages": messages }).to_string())
    }

    /// Strict parser: single `messages` key, non-empty array of known roles/strings, and at least one user turn.
    /// Returns `None` so callers fail closed.
    pub fn from_payload(payload: &crate::command::JsonPayload) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(&payload.0).ok()?;
        let object = value.as_object().filter(|o| o.len() == 1)?;
        let messages = object
            .get("messages")?
            .as_array()
            .filter(|m| !m.is_empty())?;
        let mut turns = Vec::with_capacity(messages.len());
        for message in messages {
            let fields = message.as_object()?;
            if fields
                .keys()
                .any(|k| !matches!(k.as_str(), "role" | "content" | "name"))
            {
                return None;
            }
            turns.push(ChatTurn {
                role: TurnRole::parse(fields.get("role")?.as_str()?)?,
                content: fields.get("content")?.as_str()?.to_owned(),
                name: match fields.get("name") {
                    None => None,
                    Some(name) => Some(name.as_str()?.to_owned()),
                },
            });
        }
        let body = Self { turns };
        body.current().map(|_| body)
    }

    /// Index of the last user turn.
    pub fn current(&self) -> Option<usize> {
        self.turns.iter().rposition(|t| t.role == TurnRole::User)
    }
}
