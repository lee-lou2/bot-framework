//! Built-in host tools exposed to models and recorded in approvals and ledgers.
use crate::{
    command::JsonPayload,
    id::{AgentId, Revision},
    profile::{Scopes, Tool},
    state::Risk,
};
use serde_json::{Value, json};

/// Built-in tool revision referenced by issued approvals and receipts.
pub const REVISION: &str = "builtin-1";

/// Maximum lookahead for one-shot `schedule.create` (5 leap years, 1,830 days).
pub const MAX_SCHEDULE_AHEAD_MS: i64 = 5 * 366 * 24 * 3600 * 1000;

/// Spec shared between model tool listings and broker declarations; each tool requires a scope matching its ID.
pub struct ModelTool {
    pub id: &'static str,
    pub class: Risk,
    /// Touches external networks or resources; triggers approval escalation after private memory reads.
    pub external: bool,
    /// Restricted to `profile.yaml` `super_admins`.
    pub super_admin: bool,
    pub description: &'static str,
    pub schema: &'static str,
}

impl ModelTool {
    pub fn declaration(&self) -> Tool {
        Tool {
            server: "builtin".into(),
            revision: Revision(REVISION.into()),
            class: self.class,
            scopes: Scopes::from([self.id.to_owned()]),
            input: JsonPayload(self.schema.into()),
            description: None,
            env: Default::default(),
            secrets: Default::default(),
            egress: Default::default(),
            effects: false,
            external: self.external,
            destination: None,
            definition: None,
            mcp_name: None,
            timeout_secs: None,
        }
    }
}

pub const SHARE: ModelTool = ModelTool {
    id: "memory.share",
    class: Risk::Irreversible,
    external: false,
    super_admin: false,
    description: "Share one private memory entry to the profile from your own DM or CLI. Requires the owner to review the exact body and approve.",
    schema: r#"{"additionalProperties":false,"properties":{"id":{"maxLength":128,"minLength":1,"type":"string"}},"required":["id"],"type":"object"}"#,
};
pub const SHARE_TOOL: &str = SHARE.id;

pub const CREATE: ModelTool = ModelTool {
    id: "schedule.create",
    class: Risk::Write,
    external: false,
    super_admin: false,
    description: "Schedule a follow-up task. Provide the task in instruction and either at (UTC RFC 3339 timestamp) or cron (5-field, optional timezone). Set quiet to omit replies when there is nothing to report. Defaults to the current conversation; for another recipient, specify a registered binding and conversation in destination. The owner confirms the recipient and instruction on creation and approves each delivery draft.",
    schema: r#"{"additionalProperties":false,"properties":{"at":{"type":"string"},"cron":{"type":"string"},"destination":{"additionalProperties":false,"properties":{"binding":{"type":"string"},"conversation":{"additionalProperties":false,"properties":{"id":{"type":"string"},"kind":{"enum":["direct","channel"],"type":"string"},"thread":{"type":["string","null"]}},"required":["kind","id"],"type":"object"}},"required":["binding","conversation"],"type":"object"},"instruction":{"maxLength":8192,"minLength":1,"type":"string"},"quiet":{"type":"boolean"},"timezone":{"type":"string"}},"required":["instruction"],"type":"object"}"#,
};
pub const CREATE_TOOL: &str = CREATE.id;

pub const LIST: ModelTool = ModelTool {
    id: "schedule.list",
    class: Risk::Read,
    external: false,
    super_admin: false,
    description: "List your active schedules with each schedule's ID, timing, and quiet setting.",
    schema: r#"{"additionalProperties":false,"properties":{},"type":"object"}"#,
};
pub const LIST_TOOL: &str = LIST.id;

pub const CANCEL: ModelTool = ModelTool {
    id: "schedule.cancel",
    class: Risk::Write,
    external: false,
    super_admin: false,
    description: "Cancel one of your active schedules by its ID.",
    schema: r#"{"additionalProperties":false,"properties":{"id":{"maxLength":128,"minLength":1,"type":"string"}},"required":["id"],"type":"object"}"#,
};
pub const CANCEL_TOOL: &str = CANCEL.id;

pub const SKILL_INSPECT: ModelTool = ModelTool {
    id: "skill.inspect",
    class: Risk::Read,
    external: false,
    super_admin: true,
    description: "Super-admin only. Inspect the current profile's agent structure, descriptions, skill size budget (64 KiB limit), skill list, registered MCP servers, and secret names (without values). Pass skill to also return that skill's body.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"skill":{"maxLength":63,"minLength":1,"type":"string"}},"type":"object"}"#,
};
pub const SKILL_INSPECT_TOOL: &str = SKILL_INSPECT.id;

pub const SKILL_CREATE: ModelTool = ModelTool {
    id: "skill.create",
    class: Risk::Write,
    external: false,
    super_admin: true,
    description: "Super-admin only. Create a new skill (SKILL.md) shared across the profile (omit agent or set null) or scoped to a specific agent, then validate and activate immediately.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"body":{"maxLength":65536,"minLength":1,"type":"string"},"description":{"maxLength":1024,"minLength":1,"type":"string"},"name":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"}},"required":["name","description","body"],"type":"object"}"#,
};
pub const SKILL_CREATE_TOOL: &str = SKILL_CREATE.id;

pub const SKILL_UPDATE: ModelTool = ModelTool {
    id: "skill.update",
    class: Risk::Write,
    external: false,
    super_admin: true,
    description: "Super-admin only. Update an existing skill's description or body while keeping its tool declarations, then validate and activate immediately.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"body":{"maxLength":65536,"minLength":1,"type":"string"},"description":{"maxLength":1024,"minLength":1,"type":"string"},"name":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"}},"required":["name"],"type":"object"}"#,
};
pub const SKILL_UPDATE_TOOL: &str = SKILL_UPDATE.id;

pub const SKILL_DELETE: ModelTool = ModelTool {
    id: "skill.delete",
    class: Risk::Write,
    external: false,
    super_admin: true,
    description: "Super-admin only. Delete an existing skill folder, then validate and activate immediately.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"name":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"}},"required":["name"],"type":"object"}"#,
};
pub const SKILL_DELETE_TOOL: &str = SKILL_DELETE.id;

pub const MCP_ADD: ModelTool = ModelTool {
    id: "mcp.add",
    class: Risk::Write,
    external: true,
    super_admin: true,
    description: "Super-admin only. Connect a remote Streamable HTTP or local stdio MCP server to host settings, the profile, and a skill, then validate and activate immediately. Secret values are stored encrypted in SOPS/keychain only and never written to plaintext files.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"alias":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"},"auth_secret":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"},"command":{"items":{"minLength":1,"type":"string"},"minItems":1,"type":"array"},"default_class":{"enum":["read","write","irreversible"],"type":"string"},"description":{"maxLength":1024,"minLength":1,"type":"string"},"instructions":{"maxLength":65536,"minLength":1,"type":"string"},"only_tools":{"items":{"minLength":1,"type":"string"},"type":"array"},"roles":{"items":{"maxLength":63,"minLength":1,"type":"string"},"type":"array"},"root":{"minLength":1,"type":"string"},"secret_value":{"minLength":1,"type":"string"},"secret_values":{"additionalProperties":{"minLength":1,"type":"string"},"type":"object"},"skill":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"},"stdio_tools":{"items":{"additionalProperties":false,"properties":{"class":{"enum":["read","write","irreversible"],"type":"string"},"description":{"maxLength":4096,"type":"string"},"egress":{"items":{"type":"string"},"type":"array"},"env":{"items":{"type":"string"},"type":"array"},"input":{"type":"object"},"name":{"maxLength":63,"minLength":1,"type":"string"},"secrets":{"additionalProperties":{"maxLength":63,"minLength":1,"type":"string"},"type":"object"},"timeout_secs":{"maximum":900,"minimum":1,"type":"integer"}},"required":["name","class","input"],"type":"object"},"type":"array"},"tool_classes":{"additionalProperties":{"enum":["read","write","irreversible"],"type":"string"},"type":"object"},"transport":{"enum":["http","stdio"],"type":"string"},"url":{"minLength":1,"type":"string"}},"required":["alias","transport"],"type":"object"}"#,
};
pub const MCP_ADD_TOOL: &str = MCP_ADD.id;

pub const MCP_REMOVE: ModelTool = ModelTool {
    id: "mcp.remove",
    class: Risk::Write,
    external: false,
    super_admin: true,
    description: "Super-admin only. Remove a registered MCP server and its skill tool bindings, then validate and activate immediately. When remove_secrets is true, also delete unused secrets from the encrypted store.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"alias":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"},"delete_skill":{"type":"boolean"},"remove_secrets":{"type":"boolean"},"skill":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"}},"required":["alias"],"type":"object"}"#,
};
pub const MCP_REMOVE_TOOL: &str = MCP_REMOVE.id;

const MODEL_TOOLS: &[ModelTool] = &[
    SHARE,
    CREATE,
    LIST,
    CANCEL,
    SKILL_INSPECT,
    SKILL_CREATE,
    SKILL_UPDATE,
    SKILL_DELETE,
    MCP_ADD,
    MCP_REMOVE,
];

pub fn model_tools(scopes: &Scopes) -> impl Iterator<Item = &'static ModelTool> {
    model_tools_for(scopes, false)
}

/// Filters built-in tools by agent scope ceiling and super-admin status; the broker re-checks requester and revision permissions on each call.
pub fn model_tools_for(
    scopes: &Scopes,
    is_super_admin: bool,
) -> impl Iterator<Item = &'static ModelTool> {
    MODEL_TOOLS
        .iter()
        .filter(move |spec| (!spec.super_admin || is_super_admin) && scopes.contains(spec.id))
}

pub fn manage_tool(id: &str) -> Option<&'static ModelTool> {
    MODEL_TOOLS
        .iter()
        .find(|spec| spec.super_admin && spec.id == id)
}

/// Host-only tool for owner approval of scheduled delivery drafts.
pub const DELIVER_TOOL: &str = "schedule.deliver";
pub const DELEGATE_TOOL: &str = "delegate";
pub const DELEGATE_DESCRIPTION: &str =
    "Delegate work to a direct child agent and receive its result.";

pub fn delegate_schema<'a>(children: impl IntoIterator<Item = &'a AgentId>) -> Value {
    let ids: Vec<&str> = children.into_iter().map(|id| id.0.as_str()).collect();
    json!({
        "additionalProperties": false,
        "properties": {
            "agent": {"enum": ids, "type": "string"},
            "instruction": {"minLength": 1, "type": "string"},
        },
        "required": ["agent", "instruction"],
        "type": "object",
    })
}

pub fn reserved(id: &str) -> bool {
    matches!(id, DELIVER_TOOL | DELEGATE_TOOL) || MODEL_TOOLS.iter().any(|tool| id == tool.id)
}

/// Body kind redacted in storage and re-verified against the approval fingerprint in the admin API.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum CardBody {
    None,
    Share,
    Draft,
}

pub fn card_body(id: &str) -> CardBody {
    match id {
        SHARE_TOOL => CardBody::Share,
        DELIVER_TOOL => CardBody::Draft,
        _ => CardBody::None,
    }
}
