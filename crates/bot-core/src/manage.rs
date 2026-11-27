//! Super-admin runtime skill and MCP management port (implemented by `bot-cli`).
use crate::{
    command::JsonPayload,
    id::{AgentId, PrincipalId, ProfileId, Revision},
    port::Reply,
};

/// Management request forwarded by the broker after super-admin, scope, and approval checks.
#[derive(Clone, Debug)]
pub struct ManageCall {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub principal: PrincipalId,
    pub tool: String,
    /// Canonical JSON arguments; never log, audit, or render secret values on approval cards.
    pub args: JsonPayload,
}

#[derive(Clone, Debug)]
pub struct ManageOutcome {
    pub payload: JsonPayload,
    pub activated: Option<Revision>,
}

/// Fixed-vocabulary failure reasons; `code` is audited and must never carry free-form text or secrets.
#[derive(Clone, Debug)]
pub enum ManageError {
    /// Rule or validation failure; `message` must be a safe summary for the model.
    Rejected {
        code: &'static str,
        message: String,
    },
    Conflict,
    Unavailable(&'static str),
}

pub trait ProfileManager: Send + Sync {
    fn execute(&self, call: ManageCall) -> Reply<'_, Result<ManageOutcome, ManageError>>;
}
