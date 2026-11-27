//! Model runner port (`ModelRunner`) and implementations for `Step` (API/demo) and `Loop` (VM-sandboxed CLI) execution.
//!
//! `Step` runners return `ModelStep::Calls` for the worker to execute via `Broker::call`; `Loop`
//! runners run inside a host-only VM with egress restricted to the per-attempt broker relay.
//! Errors carry fixed vocabulary codes only.

mod agy;
pub mod api;
mod claude_code;
mod cli;
mod codex;
pub mod container;
mod demo;
mod driver;
mod exchange;
mod prompt;
mod relay;
#[cfg(test)]
mod sandbox;
pub mod session;

pub use agy::AgyRunner;
pub use api::{ApiClient, ApiEndpoint, ApiKind, ApiRunner, ApiSettings, Effort, LimitField};
pub use claude_code::ClaudeCodeRunner;
pub use cli::{CliConfig, CliKind, CliModel};
pub use codex::CodexRunner;
pub use demo::DemoModel;
pub use driver::{Event, Output, Parser, classify, is_transient};
pub use exchange::{CallEnd, DoneCall, Exchange};
pub use relay::{RelayChannel, RelayGrant, TOKEN_ENV};
#[cfg(test)]
pub(crate) use sandbox::{FakeSandbox, Script};
pub use session::{PortFuture, Session, SessionError, SessionPort, Slot, ToolOutput};

use bot_core::{
    command::{Failure, JsonPayload, RunOutcome, TokenUsage},
    id::{Attempt, CallId, PrincipalId, RunId, ToolId},
    profile::Builtin,
    state::Risk,
};
use std::{collections::BTreeSet, fmt, future::Future, pin::Pin, time::Duration};
use tokio::sync::watch;

pub type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<ModelStep, ModelError>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerKind {
    /// Worker drives turns one at a time and relays `ModelStep::Calls` through `Broker::call`.
    Step,
    /// Runner drives its own tool loop via `ModelRequest::relay` and returns a single `Final` step.
    Loop,
}

/// Non-secret provider/upstream model metadata attached to OTel GenAI spans.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelMeta {
    pub provider: &'static str,
    pub model: String,
}

pub trait ModelRunner: Send + Sync {
    fn kind(&self) -> RunnerKind;
    fn step(&self, request: ModelRequest) -> StepFuture<'_>;
    fn meta(&self, _alias: &str) -> Option<ModelMeta> {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub role: Role,
    pub speaker: Option<PrincipalId>,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    Skill,
    Delegate,
}

/// Advisory tool description shown to the model; authorization is re-enforced per call by the broker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolSpec {
    pub id: ToolId,
    pub kind: ToolKind,
    pub class: Risk,
    pub description: String,
    pub input: JsonPayload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub total: Duration,
    pub idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            total: Duration::from_secs(600),
            idle: Duration::from_secs(120),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub attempt: Attempt,
    pub model: String,
    pub system: String,
    pub conversation: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub exchanges: Vec<Exchange>,
    pub limits: Limits,
    pub cancel: watch::Receiver<bool>,
    /// Read-only VM built-in tools explicitly enabled in `AGENT.md` (`Loop` only).
    pub builtin: BTreeSet<Builtin>,
    /// Per-attempt broker front channel (`Loop` only; required for CLI model/tool egress).
    pub relay: Option<RelayChannel>,
    /// Optional durable session port for approval/delegation resume (`ClaudeCodeRunner` only).
    pub session: Option<Session>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl std::ops::Add for Usage {
    type Output = Usage;

    fn add(self, other: Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingCall {
    pub call: CallId,
    pub tool: ToolId,
    pub args: JsonPayload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelStep {
    Final {
        text: String,
        usage: Usage,
        outcome: RunOutcome,
    },
    Calls {
        text: String,
        calls: Vec<PendingCall>,
        usage: Usage,
        /// Measured provider token counts for this turn only (`None` if unreported; never zero-filled).
        measured: Option<TokenUsage>,
        /// Opaque provider state persisted in turn checkpoints and echoed back to the issuing provider.
        state: Option<String>,
    },
}

/// Model execution failure carrying fixed vocabulary codes only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelError {
    Cancelled,
    Timeout,
    Parked,
    Lost,
    Failed { code: &'static str, transient: bool },
}

impl ModelError {
    pub fn code(&self) -> &'static str {
        match self {
            ModelError::Cancelled => "cancelled",
            ModelError::Timeout => "timeout",
            ModelError::Parked => "parked",
            ModelError::Lost => "lost",
            ModelError::Failed { code, .. } => code,
        }
    }

    pub fn transient(&self) -> bool {
        match self {
            ModelError::Cancelled | ModelError::Parked | ModelError::Lost => false,
            ModelError::Timeout => true,
            ModelError::Failed { transient, .. } => *transient,
        }
    }

    pub fn failure(&self) -> Failure {
        if self.transient() {
            Failure::Transient(self.code())
        } else {
            Failure::Permanent(self.code())
        }
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "model step failed: {}", self.code())
    }
}

impl std::error::Error for ModelError {}

pub fn call_id(run: &RunId, step: usize, index: usize) -> CallId {
    CallId(format!("{}:{step}:{index}", run.0))
}
