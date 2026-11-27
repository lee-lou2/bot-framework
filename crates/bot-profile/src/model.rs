//! Internal intermediate representation used across parsing, assembly, resolution, and snapshot conversion.
use crate::schema::Schema;
use bot_core::{
    command::JsonPayload,
    cron::Cron,
    id::{AgentId, BindingId, PrincipalId, Revision, SecretName, ToolId},
    profile::{Adapter, Builtin, Destination, Document, MemorySeed, Policy},
    state::Risk,
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub struct Member {
    pub identity: PrincipalId,
    pub kind: bot_core::identity::IdentityKind,
    pub roles: BTreeSet<String>,
    /// Union of role scopes intersected with the profile policy ceiling.
    pub scopes: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub id: BindingId,
    pub agent: AgentId,
    pub kind: Adapter,
}

/// Canonical entrance key, unique across all active profiles.
pub fn entrance(kind: &Adapter) -> String {
    match kind {
        Adapter::Cli { account } => format!("cli:{account}"),
        Adapter::SlackChannel {
            workspace, channel, ..
        } => {
            format!("slack:{workspace}:channel:{channel}")
        }
        Adapter::SlackDm { workspace } => format!("slack:{workspace}:dm"),
        Adapter::Http { endpoint } => format!("http:{endpoint}"),
        // Multiple OpenAI models can share an endpoint alias within a single profile.
        Adapter::OpenAi {
            endpoint, model, ..
        } => format!("http:{endpoint}/{model}"),
    }
}

/// Canonical ownership key; an OpenAI endpoint alias is owned as a whole by one profile.
pub fn owner_key(kind: &Adapter) -> String {
    match kind {
        Adapter::OpenAi { endpoint, .. } => format!("http:{endpoint}"),
        other => entrance(other),
    }
}

pub fn adapter_name(kind: &Adapter) -> &'static str {
    match kind {
        Adapter::Cli { .. } => "cli",
        Adapter::SlackChannel { .. } | Adapter::SlackDm { .. } => "slack",
        Adapter::Http { .. } => "http",
        Adapter::OpenAi { .. } => "openai",
    }
}

#[derive(Clone, Debug)]
pub struct Tool {
    pub id: ToolId,
    pub name: String,
    pub server: String,
    pub class: Risk,
    pub scopes: BTreeSet<String>,
    pub external: bool,
    pub effects: bool,
    pub destination: Option<Destination>,
    pub env: BTreeSet<String>,
    /// Environment variable name -> logical secret name.
    pub secrets: BTreeMap<String, SecretName>,
    pub egress: BTreeSet<String>,
    pub input: JsonPayload,
    pub description: Option<String>,
    pub revision: Revision,
    pub schema: Arc<Schema>,
    /// Lock definition SHA-256 (`bot_core::mcp::definition_sha256`) when `input` came from `tools.lock.yaml`.
    pub definition: Option<String>,
    /// Remote MCP tool name when it differs from `name`; `None` otherwise.
    pub mcp_name: Option<String>,
    /// Per-call timeout in seconds; `None` defaults to 60s.
    pub timeout_secs: Option<u32>,
}

impl Tool {
    /// Effective risk; tools with explicit recipient arguments are elevated to `Irreversible`.
    pub fn risk(&self) -> Risk {
        match self.destination {
            Some(Destination::Explicit { .. }) => Risk::Irreversible,
            _ => self.class,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
    pub instructions: Document,
    pub tools: Vec<Arc<Tool>>,
}

#[derive(Clone, Debug)]
pub enum Source {
    Cron {
        cron: Cron,
        beat: bool,
        principal: PrincipalId,
        approver: Option<PrincipalId>,
    },
    Message {
        binding: BindingId,
    },
    ThreadReply {
        binding: BindingId,
    },
    Reaction {
        binding: BindingId,
    },
    Http {
        method: String,
        path: String,
        binding: BindingId,
        sync: bool,
    },
}

impl Source {
    /// Candidate group key for trigger ordering (`None` for cron).
    pub fn group(&self) -> Option<String> {
        match self {
            Self::Cron { .. } => None,
            Self::Message { binding } => Some(format!("message:{}", binding.0)),
            Self::ThreadReply { binding } => Some(format!("thread-reply:{}", binding.0)),
            Self::Reaction { binding } => Some(format!("reaction:{}", binding.0)),
            Self::Http {
                method,
                path,
                binding,
                ..
            } => Some(format!("http:{}:{method} {path}", binding.0)),
        }
    }

    pub fn binding(&self) -> Option<&BindingId> {
        match self {
            Self::Cron { .. } => None,
            Self::Message { binding }
            | Self::ThreadReply { binding }
            | Self::Reaction { binding }
            | Self::Http { binding, .. } => Some(binding),
        }
    }
}

#[derive(Clone, Debug)]
pub enum TriggerTarget {
    Tool {
        tool: ToolId,
        args: Map<String, Value>,
    },
    Instruction(String),
}

#[derive(Clone, Debug)]
pub struct Match {
    pub path: String,
    pub value: Value,
}

#[derive(Clone, Debug)]
pub struct Trigger {
    pub id: String,
    pub on: Source,
    pub target: TriggerTarget,
    pub order: i64,
    pub private: bool,
    pub private_pass: bool,
    pub matches: Vec<Match>,
    /// Tool argument name -> dotted envelope path.
    pub input: BTreeMap<String, String>,
    pub preapproved: Option<bot_core::grant::Preapproval>,
    pub human_approval: Option<bot_core::grant::HumanApproval>,
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub id: AgentId,
    pub parent: Option<AgentId>,
    /// Effective model alias inherited from parent or profile default.
    pub model: String,
    /// Delegation summary from own front matter (not inherited), falling back to the leaf name.
    pub description: String,
    /// Read-only built-in tools explicitly enabled on this agent (not inherited).
    pub builtin: BTreeSet<Builtin>,
    /// Ordered instruction parts: `rules.md` -> ancestors -> self.
    pub parts: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub skills: Vec<Arc<Skill>>,
    /// Ordered knowledge documents: profile -> ancestors -> self.
    pub knowledge: Vec<Document>,
    pub triggers: Vec<Trigger>,
    pub memory: Vec<MemorySeed>,
    /// Effective scope ceiling: `parent.ceiling ∩ policy.agents[id]`.
    pub ceiling: BTreeSet<String>,
}

impl Agent {
    pub fn skill(&self, name: &str) -> Option<&Arc<Skill>> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn tool(&self, id: &ToolId) -> Option<&Arc<Tool>> {
        let (skill, _) = id.0.split_once('.')?;
        self.skill(skill)?.tools.iter().find(|t| t.id == *id)
    }

    pub fn tools(&self) -> impl Iterator<Item = &Arc<Tool>> {
        self.skills.iter().flat_map(|s| s.tools.iter())
    }
}

pub struct ProfileParts {
    pub id: String,
    pub timezone: String,
    pub model: String,
    pub roles: BTreeMap<String, BTreeSet<String>>,
    pub members: BTreeMap<PrincipalId, Member>,
    pub super_admins: BTreeSet<PrincipalId>,
    pub bindings: BTreeMap<BindingId, Binding>,
    pub secrets: BTreeSet<String>,
    pub policy: Policy,
}

pub struct Built {
    pub parts: ProfileParts,
    pub agents: BTreeMap<AgentId, Agent>,
    pub root: AgentId,
}
