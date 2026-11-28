//! Read-only contracts for validated profile snapshots (excludes host credential values).
use crate::{
    command::JsonPayload,
    id::*,
    state::{Risk, Visibility},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub type Scopes = BTreeSet<String>;

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: ProfileId,
    pub revision: Revision,
    pub policy_revision: Revision,
    pub timezone: String,
    pub model: String,
    pub root: AgentId,
    pub agents: BTreeMap<AgentId, Agent>,
    pub bindings: BTreeMap<BindingId, Binding>,
    pub roles: BTreeMap<String, Scopes>,
    pub members: BTreeMap<PrincipalId, BTreeSet<String>>,
    /// Principals permitted to use runtime `skill.*`/`mcp.*` tools (must be non-service members).
    pub super_admins: BTreeSet<PrincipalId>,
    pub policy: Policy,
    pub secrets: Vec<SecretName>,
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub scopes: Scopes,
    pub agents: BTreeMap<AgentId, Scopes>,
    pub approve: Scopes,
}

/// Read-only built-in tools a model runner may expose.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Builtin {
    Read,
    Grep,
    Glob,
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub description: String,
    /// Declared explicitly in this `AGENT.md`; not inherited from parent and empty by default.
    pub builtin: BTreeSet<Builtin>,
    pub parent: Option<AgentId>,
    /// `model`, `instructions`, `env`, `scopes`, `skills`, and `knowledge` include ancestor inheritance.
    pub model: String,
    pub instructions: String,
    pub env: BTreeMap<String, String>,
    pub scopes: Scopes,
    pub skills: BTreeMap<String, Skill>,
    pub knowledge: Vec<Document>,
    /// Imported into the DB at startup; never mixed into model instructions or knowledge search.
    pub memory_seeds: Vec<MemorySeed>,
    /// Owned solely by the declaring agent (not inherited).
    pub triggers: Vec<Trigger>,
}

#[derive(Clone, Debug)]
pub struct MemorySeed {
    pub document: Document,
    pub owner: PrincipalId,
    pub visibility: Visibility,
}

#[derive(Clone, Debug)]
pub struct Document {
    pub relative_path: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub description: Option<String>,
    pub instructions: Document,
    /// Keyed by full `skill.tool` ID; visibility is independent per agent.
    pub tools: BTreeMap<ToolId, Tool>,
}

#[derive(Clone, Debug)]
pub struct Tool {
    pub server: String,
    pub revision: Revision,
    pub class: Risk,
    pub scopes: Scopes,
    pub input: JsonPayload,
    /// Lockfile description included in the definition hash.
    pub description: Option<String>,
    /// Names selected from the calling agent's inherited non-secret configuration.
    pub env: BTreeSet<String>,
    pub secrets: BTreeMap<String, SecretName>,
    pub egress: BTreeSet<String>,
    pub effects: bool,
    pub external: bool,
    pub destination: Option<Destination>,
    /// Fixed SHA-256 hex digest (`mcp::definition_sha256`) of name, description, and input schema from `tools.lock.yaml`.
    /// Verified against the live server definition before execution and folded into `revision` so changes invalidate approvals.
    pub definition: Option<String>,
    /// Upstream MCP tool name (`tools.yaml` `name`); defaults to the `tool` segment of `skill.tool` and is folded into `revision`.
    pub mcp_name: Option<String>,
    /// Per-call broker timeout in seconds (`1..=MAX_TOOL_CALL_SECS`, default `DEFAULT_TOOL_CALL_SECS`); folded into `revision`.
    pub timeout_secs: Option<u32>,
}

/// Default per-call timeout and default host ceiling when `limits.tool_call_max_secs` is unset.
pub const DEFAULT_TOOL_CALL_SECS: u32 = 60;
pub const MAX_TOOL_CALL_SECS: u32 = 900;

impl Tool {
    /// Prefers lockfile description, falling back to top-level schema `description`, truncated to 4 KiB UTF-8 for display.
    pub fn model_description(&self) -> Option<String> {
        let mut text = self
            .description
            .clone()
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                let input: serde_json::Value = serde_json::from_str(&self.input.0).ok()?;
                input
                    .get("description")?
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                    .map(str::to_owned)
            })?;
        text.truncate(text.floor_char_boundary(4 * 1024));
        Some(text)
    }

    pub fn call_secs(&self) -> u32 {
        self.timeout_secs.unwrap_or(DEFAULT_TOOL_CALL_SECS)
    }
}

#[derive(Clone, Debug)]
pub enum Destination {
    Reply,
    Explicit { recipient: String, content: String },
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub agent: AgentId,
    pub source: Adapter,
}

#[derive(Clone, Debug)]
pub enum Adapter {
    Cli {
        account: String,
    },
    SlackChannel {
        workspace: String,
        channel: String,
        /// Non-empty set of accepted channel events (defaults to mentions only).
        events: std::collections::BTreeSet<crate::slack::SlackEvent>,
    },
    SlackDm {
        workspace: String,
    },
    TelegramDm {
        bot: String,
    },
    /// Legacy webhook ingress (`kind: webhook`).
    Http {
        endpoint: String,
    },
    /// OpenAI-compatible model binding on host alias `endpoint` (`kind: openai`); triggers cannot target it.
    /// `approvers` maps HTTP principals (`http:<endpoint>/<sub>`) to human approval routes; unmapped principals cannot run approval-gated tools.
    OpenAi {
        endpoint: String,
        model: String,
        approvers: BTreeMap<PrincipalId, ApprovalRoute>,
    },
}

/// Human approval route per HTTP principal on an OpenAI binding; never elevates the HTTP principal's scopes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalRoute {
    /// Approved in the authenticated Admin UI by a `local:*` profile member.
    Local { person: PrincipalId },
    /// Approved via DM to a `slack:<workspace>/<user>` member, anchored to a channel `binding` in the same profile and workspace.
    Slack {
        person: PrincipalId,
        binding: BindingId,
    },
}

impl ApprovalRoute {
    pub fn person(&self) -> &PrincipalId {
        match self {
            Self::Local { person } | Self::Slack { person, .. } => person,
        }
    }
}

impl Snapshot {
    pub fn is_super_admin(&self, principal: &PrincipalId) -> bool {
        self.super_admins.contains(principal)
    }

    /// Resolves the human approval route for an authenticated HTTP `requester` on an OpenAI `binding` within this snapshot.
    /// Callers must verify pinned and current revisions yield the same route; `None` blocks approval-gated tools.
    pub fn approval_route(
        &self,
        binding: &BindingId,
        requester: &PrincipalId,
    ) -> Option<ApprovalRoute> {
        let Adapter::OpenAi {
            endpoint,
            approvers,
            ..
        } = &self.bindings.get(binding)?.source
        else {
            return None;
        };
        let from_endpoint = requester
            .0
            .strip_prefix("http:")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(alias, _)| alias == endpoint);
        if !from_endpoint || !self.members.contains_key(requester) {
            return None;
        }
        let route = approvers.get(requester)?;
        let person = route.person();
        if !person.is_human() || !self.members.contains_key(person) {
            return None;
        }
        match route {
            ApprovalRoute::Local { .. } => person.0.starts_with("local:").then(|| route.clone()),
            ApprovalRoute::Slack {
                binding: delivery, ..
            } => {
                let workspace = person
                    .0
                    .strip_prefix("slack:")?
                    .split_once('/')
                    .map(|(workspace, _)| workspace)?;
                match &self.bindings.get(delivery)?.source {
                    Adapter::SlackChannel { workspace: w, .. } if w == workspace => {
                        Some(route.clone())
                    }
                    _ => None,
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Trigger {
    pub id: String,
    pub on: TriggerSource,
    pub order: i64,
    /// Envelope dot-path to literal JSON match value.
    pub conditions: BTreeMap<String, JsonPayload>,
    /// Tool argument name to envelope dot-path.
    pub input: BTreeMap<String, String>,
    pub private: bool,
    pub private_pass: bool,
    pub target: TriggerTarget,
    /// Operator-granted static call pre-approval (cron direct-tool triggers only).
    pub preapproved: Option<crate::grant::Preapproval>,
    /// Direct-tool trigger authorized by a designated human's message or reaction.
    pub human_approval: Option<crate::grant::HumanApproval>,
}

#[derive(Clone, Debug)]
pub enum TriggerSource {
    Cron {
        schedule: String,
        beat: bool,
        principal: PrincipalId,
        approver: Option<PrincipalId>,
    },
    Event {
        binding: BindingId,
        event: String,
    },
    Http {
        binding: BindingId,
        method: String,
        path: String,
        response: ResponseMode,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseMode {
    Accepted,
    Sync,
}

#[derive(Clone, Debug)]
pub enum TriggerTarget {
    Instruction(String),
    Tool { tool: ToolId, args: JsonPayload },
}

/// `active` swaps atomically as a set; reading a pinned older revision still enforces current authorization.
pub trait ProfileCatalog: Send + Sync {
    fn active(&self) -> Arc<BTreeMap<ProfileId, Arc<Snapshot>>>;
    fn current(&self, profile: &ProfileId) -> Option<Arc<Snapshot>> {
        self.active().get(profile).cloned()
    }
    /// Returns the exact pinned revision (including restored bundles) without altering `active`/`current`.
    fn lookup(&self, profile: &ProfileId, revision: &Revision) -> Option<Arc<Snapshot>>;
}

/// Publishes active profile sets with CAS; rejects replacement if `expected` no longer matches.
pub trait ActiveRevisions: Send + Sync {
    /// Last published set revision, or `None` before initial activation.
    fn current(&self) -> crate::port::Reply<'_, Option<Revision>>;
    /// Atomically commits CAS, memory seeds, static cron windows (bounded by one DB timestamp), and audit.
    /// Profiles omitted from `schedules` get 0 static triggers; keys outside `profiles` fail with `Invalid`. User schedules are independent.
    fn replace(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<crate::command::MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<crate::command::TriggerSpec>>,
    ) -> crate::port::Reply<'_, ()>;

    /// Like `replace`, but re-reads pinned revisions under the active row lock and returns `Stale` without writing if any is missing from `prepared`.
    fn replace_prepared(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<crate::command::MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<crate::command::TriggerSpec>>,
        prepared: BTreeSet<(ProfileId, Revision)>,
    ) -> crate::port::Reply<'_, ()>;

    /// Like `replace_prepared`, also syncing pre-approval grants in the same transaction (preserving matching fingerprints, revoking removed/changed ones permanently).
    /// Returns newly issued grants.
    fn replace_granted(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<crate::command::MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<crate::command::TriggerSpec>>,
        plan: crate::grant::GrantPlan,
    ) -> crate::port::Reply<'_, Vec<(crate::grant::GrantId, crate::grant::GrantSpec)>>;

    /// Immediate activation for live skill/MCP edits: like `replace_prepared`, without issuing or revoking `bot_grants`.
    fn replace_live(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<crate::command::MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<crate::command::TriggerSpec>>,
        prepared: BTreeSet<(ProfileId, Revision)>,
    ) -> crate::port::Reply<'_, ()>;
}

/// Immutable archive of validated profile revision bundles used to restore pinned revisions across redeploys.
pub trait ProfileBundles: Send + Sync {
    /// Stores under `(profile, revision)` where `revision` must equal the bundle SHA-256 hex (`Invalid` otherwise); differing bytes for an existing key return `Conflict`.
    fn put_bundle(
        &self,
        profile: ProfileId,
        revision: Revision,
        bundle: Vec<u8>,
    ) -> crate::port::Reply<'_, ()>;
    fn bundle(
        &self,
        profile: &ProfileId,
        revision: &Revision,
    ) -> crate::port::Reply<'_, Option<Vec<u8>>>;
    /// First-stored Unix timestamp in milliseconds, used for stable `/v1/models` `created` values.
    fn created_ms(
        &self,
        profile: &ProfileId,
        revision: &Revision,
    ) -> crate::port::Reply<'_, Option<i64>>;
    /// Reads all `(profile, revision)` pairs pinned by `target` profiles in one snapshot: `target` itself plus live references
    /// (unfinished runs, resumable dead letters, pending/unconsumed approval runs, open/unconsumed static cron windows, and active user schedule origin runs).
    fn pinned_revisions<'a>(
        &'a self,
        target: &'a BTreeMap<ProfileId, Revision>,
    ) -> crate::port::Reply<'a, BTreeSet<(ProfileId, Revision)>>;
}

/// Shared JSON Schema validator for both manifest validation and runtime tool calls.
pub trait InputValidator: Send + Sync {
    fn validate(&self, schema: &JsonPayload, args: &JsonPayload) -> Result<(), crate::port::Error>;
}

/// Tool environment names share the sandbox launcher's reserved namespace.
pub use crate::tool_env::forbidden_env_name;
