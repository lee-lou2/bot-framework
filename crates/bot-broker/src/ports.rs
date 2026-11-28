//! Internal broker ports for argument sealing and sandboxed tool execution.

use crate::resolve::Resolved;
use bot_core::{
    command::JsonPayload,
    id::*,
    port::{Error, Reply},
    profile::Tool,
    state::{Conversation, RunOrigin},
};

/// Validated and RFC 8785 canonicalized arguments; `canonical` bytes are both hashed and sent to the tool.
#[derive(Clone, Debug)]
pub struct Sealed {
    pub canonical: String,
    pub sha256: [u8; 32],
    /// Extracted `(recipient, content)` for explicit destinations; `seal` fails if unresolved.
    pub destination: Option<(String, String)>,
}

pub trait Sealer: Send + Sync {
    fn seal(&self, tool: &Tool, args: &JsonPayload) -> Result<Sealed, Error>;
}

#[derive(Clone, Debug)]
pub struct ToolJob {
    pub attempt: Attempt,
    pub principal: PrincipalId,
    /// Root intake origin propagated into `_meta["bot/trigger"]` across delegation and resume.
    pub origin: RunOrigin,
    pub call: CallId,
    pub tool: Resolved,
    /// Exact canonical argument bytes matching the approved SHA-256 digest.
    pub args: String,
    /// Non-secret agent env vars selected by `Tool.env` only when pinned and current revisions agree.
    pub env: Vec<(String, String)>,
    /// Host-verified conversation of the run (injected into local `_meta["bot/conversation"]`, never remote).
    pub conversation: Conversation,
    /// Fixed origin conversation for `Destination::Reply` (injected into local `_meta["bot/destination"]`).
    pub reply_to: Option<Conversation>,
    pub owner: String,
}

#[derive(Clone, Debug)]
pub enum Ran {
    Done(JsonPayload),
    /// Failed before `tools/call` was sent (spawn, handshake, or schema mismatch); proves non-execution.
    NotSent,
    /// Server deterministically rejected the call before execution (e.g., JSON-RPC `-32602..=-32600`).
    Rejected,
    /// Broker deterministically refused before `tools/call` (e.g., pinned definition mismatch); closes as denied.
    Refused(&'static str),
    /// Sent with unknown remote outcome (timeout, crash, disconnect, or internal server error).
    Unknown(&'static str),
    /// Executed, but output was blocked (credential leak, shape, or size violation).
    Blocked(&'static str),
}

pub trait ToolRunner: Send + Sync {
    /// Pinned executable/image revision bound into approvals; changing it invalidates prior approvals.
    fn installed(&self, server: &str) -> Option<String>;

    /// Checks whether the sandbox can run `server` (e.g., rejects `state: required` if unsupported).
    fn admit(&self, _server: &str) -> Result<(), Error> {
        Ok(())
    }

    /// Verifies declared secrets exist in the profile store before recording a call or opening an approval card.
    fn admit_secrets(&self, _profile: &ProfileId, _names: &[SecretName]) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    /// Host-side secrets required for `server` (e.g., remote bearer token), checked alongside `Tool.secrets`.
    fn server_secrets(&self, _server: &str) -> Vec<SecretName> {
        Vec::new()
    }

    fn run(&self, job: ToolJob) -> Reply<'_, Ran>;

    /// Proves all processes from an earlier attempt have exited; returns `Unavailable` if unconfirmed.
    fn reap(&self, owner: String) -> Reply<'_, ()>;

    /// Whether `reap` proves remote work stopped (`true` for local sandboxes, `false` for remote HTTP servers).
    fn reap_proves_end(&self, _server: &str) -> bool {
        true
    }
}
