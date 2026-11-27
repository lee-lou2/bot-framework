//! Shared state enums, conversation/origin metadata, `RunContext`, and `Approval` vocabulary.

use crate::{command::Work, id::*};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunState {
    Ready,
    Leased,
    Waiting,
    Retry,
    Succeeded,
    Dead,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectState {
    Started,
    Confirmed,
    Rejected,
    Held,
}

/// Operator confirmed a held step did not execute; consumed on the next successful claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectResolution {
    NotExecuted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Risk {
    Read,
    Write,
    Irreversible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Visibility {
    Private,
    Profile,
}

/// Persisted in SQL constraints; adding a variant requires a matching migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationKind {
    Direct,
    Channel,
    Cli,
    Http,
    Scheduled,
}

/// Origin recorded at authenticated root intake or schedule fire, stored separately from reply conversation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunOrigin {
    Message,
    Http,
    Schedule,
    Wake,
    Unknown,
}

impl RunOrigin {
    /// For authenticated gateways and private DM ingress only; scheduled runs set origin directly.
    pub fn from_ingress(kind: &ConversationKind, principal: &PrincipalId) -> Self {
        use crate::identity::{IdentityKind, identity_kind};
        match (kind, identity_kind(&principal.0)) {
            (ConversationKind::Cli, Some(IdentityKind::Local))
            | (ConversationKind::Direct | ConversationKind::Channel, Some(IdentityKind::Slack)) => {
                Self::Message
            }
            (ConversationKind::Http, Some(IdentityKind::Http)) => Self::Http,
            _ => Self::Unknown,
        }
    }

    /// Fixed wire value shared by MCP metadata and the ledger.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Http => "http",
            Self::Schedule => "schedule",
            Self::Wake => "wake",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_recorded(value: &str) -> Self {
        match value {
            "message" => Self::Message,
            "http" => Self::Http,
            "schedule" => Self::Schedule,
            "wake" => Self::Wake,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub kind: ConversationKind,
    pub id: String,
    pub thread: Option<String>,
}

/// Trigger IDs are unique per owning agent within a profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerRef {
    pub agent: AgentId,
    pub id: String,
}

/// Share scope is bound to the original request's profile, principal, and conversation.
#[derive(Clone, Debug)]
pub struct RunContext {
    pub attempt: Attempt,
    pub root: RunId,
    /// Root origin, preserved across delegation and resume.
    pub origin: RunOrigin,
    pub principal: PrincipalId,
    pub agent: AgentId,
    pub conversation: Conversation,
    /// Root intake binding, inherited unchanged by delegated children.
    pub binding: BindingId,
    /// OpenAI-compatible intake origin when root entered via HTTP API (inherited by children).
    pub api: Option<crate::api::ApiOrigin>,
    pub scopes: std::collections::BTreeSet<String>,
    pub private_memory_read: bool,
    pub work: Work,
    pub approver: Option<PrincipalId>,
    /// Shared by a scheduled run and its children; `None` for normal requests.
    pub trigger: Option<TriggerRef>,
}

#[derive(Clone, Debug)]
pub struct Approval {
    pub nonce: ApprovalId,
    pub attempt: Attempt,
    pub call: CallId,
    pub requester: PrincipalId,
    pub approver: PrincipalId,
    pub risk: Risk,
    pub policy_revision: Revision,
    pub tool: ToolId,
    pub tool_revision: Revision,
    pub args_sha256: [u8; 32],
    pub expires_at: std::time::SystemTime,
}
