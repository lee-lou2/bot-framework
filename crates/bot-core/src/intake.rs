//! Authenticated request status, private DM intake, and reply outbox ports.
use crate::{
    command::{HttpRoute, JsonPayload},
    id::*,
    port::{Error, Reply},
    state::*,
};
use std::borrow::Cow;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RequestKey {
    pub profile: ProfileId,
    pub binding: BindingId,
    pub principal: PrincipalId,
    pub request: RequestId,
}

pub struct RequestState {
    pub run: RunId,
    pub state: RunState,
    pub result: Option<JsonPayload>,
    /// HTTP route and response mode saved at admission; `None` for legacy rows (queried as accepted).
    pub http: Option<HttpRoute>,
}

/// Non-sensitive summary of an active run tree for Slack progress and stop cards (no bodies, args, or results).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunProgress {
    pub state: RunState,
    pub cancel_requested: bool,
    /// Active delegated sub-agent (e.g., `main/proxy`), or `None` when the root is running.
    pub active_agent: Option<String>,
    pub active_tool: Option<String>,
    pub waiting_approval: bool,
}

/// Private DM call mapped directly to a tool; the private ledger never persists raw input.
pub struct PrivateCall {
    pub key: RequestKey,
    pub revision: Revision,
    pub conversation: Conversation,
    pub agent: AgentId,
    pub trigger: String,
    pub tool: ToolId,
    pub args: JsonPayload,
    /// Digest of the full envelope and verified event identity; used only in memory as HMAC input.
    pub envelope_sha256: [u8; 32],
}

/// `Pass` is returned only when a `private_pass: true` trigger confirms the input is non-sensitive and hands it to normal chat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivateDecision {
    Consumed,
    Pass,
}

pub trait PrivateIntake: Send + Sync {
    /// Executes at most once per request; signals retry while re-claim still requires raw input.
    fn receive(&self, call: PrivateCall) -> Reply<'_, PrivateDecision>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushTransportId(Cow<'static, str>);

impl PushTransportId {
    pub const SLACK: Self = Self(Cow::Borrowed("slack"));
    pub const TELEGRAM: Self = Self(Cow::Borrowed("telegram"));

    pub fn new(name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        let bytes = name.as_bytes();
        if (1..=64).contains(&bytes.len())
            && bytes[0].is_ascii_lowercase()
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        {
            Ok(Self(Cow::Owned(name)))
        } else {
            Err(Error::Invalid)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryScope {
    pub transport: PushTransportId,
    pub binding: Option<(ProfileId, BindingId)>,
}

impl DeliveryScope {
    pub const fn all(transport: PushTransportId) -> Self {
        Self {
            transport,
            binding: None,
        }
    }

    pub fn contains(&self, item: &Outgoing) -> bool {
        self.transport == item.transport
            && self.binding.as_ref().is_none_or(|(profile, binding)| {
                *profile == item.key.profile && *binding == item.binding
            })
    }
}

/// Reply reservation independent of worker leases; `call` stays fixed across retries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryKey {
    pub profile: ProfileId,
    pub run: RunId,
    pub seq: u32,
    pub generation: Generation,
}

pub struct Outgoing {
    pub key: DeliveryKey,
    pub transport: PushTransportId,
    pub call: CallId,
    pub binding: BindingId,
    pub principal: PrincipalId,
    pub destination: Conversation,
    pub body: JsonPayload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotSentReason {
    /// Connection failed before writing request bytes.
    BeforeWrite,
    /// Verified destination rejected the request for rate limiting before execution.
    RateLimited,
}

pub enum DeliveryOutcome {
    Confirmed {
        receipt: String,
    },
    /// Confirmed non-execution transient failure; `retry_after` is a lower bound.
    NotSent {
        reason: NotSentReason,
        retry_after: Option<std::time::Duration>,
    },
    Rejected,
    Held,
}

pub trait Outbox: Send + Sync {
    /// Claims pending deliveries with a lease; excludes ambiguous in-flight `sending` entries.
    fn claim<'a>(&'a self, scope: &'a DeliveryScope, limit: u32) -> Reply<'a, Vec<Outgoing>>;
    /// Atomically verifies lease and posting eligibility and marks `sending`; only the first sender gets `Ok`.
    fn begin(&self, key: DeliveryKey, checked_revision: Revision) -> Reply<'_, ()>;
    /// Verifies generation and records outcome, receipt, and audit entry.
    fn settle(&self, key: DeliveryKey, outcome: DeliveryOutcome) -> Reply<'_, ()>;
}
