//! Authenticated, normalized adapter inputs passed into `Ingress::admit`.
use crate::entrance::Real;
use bot_core::{
    id::{PrincipalId, RequestId},
    state::{Conversation, ConversationKind},
};
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Message,
    ThreadReply,
    Reaction,
    Http { method: String, path: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Respond {
    Accepted,
    Sync,
}

impl From<Respond> for bot_core::profile::ResponseMode {
    fn from(respond: Respond) -> Self {
        match respond {
            Respond::Accepted => Self::Accepted,
            Respond::Sync => Self::Sync,
        }
    }
}

impl From<&bot_core::profile::ResponseMode> for Respond {
    fn from(mode: &bot_core::profile::ResponseMode) -> Self {
        match mode {
            bot_core::profile::ResponseMode::Accepted => Self::Accepted,
            bot_core::profile::ResponseMode::Sync => Self::Sync,
        }
    }
}

/// Do not log with `{:?}` because `body` holds raw request content.
pub struct Inbound {
    pub real: Real,
    pub principal: PrincipalId,
    /// Stable idempotency key across retries (Slack event ID or caller-supplied key).
    pub request: RequestId,
    pub conversation: Conversation,
    /// Access-scoped attachment references (never fetched by the gateway).
    pub attachments: Vec<String>,
    pub body: Value,
    pub event: Event,
}

pub fn kind_name(kind: &ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Direct => "direct",
        ConversationKind::Channel => "channel",
        ConversationKind::Cli => "cli",
        ConversationKind::Http => "http",
        ConversationKind::Scheduled => "scheduled",
    }
}

/// Validates request IDs: 1-128 ASCII alphanumeric or `. _ : -` characters.
pub fn valid_request_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_short_and_delimiter_free() {
        for ok in ["a", "Ev0123ABC", "req-1.2_3:4", &"x".repeat(128)] {
            assert!(valid_request_id(ok), "{ok}");
        }
        for bad in ["", " ", "a b", "a/b", "a\n", "café", &"x".repeat(129)] {
            assert!(!valid_request_id(bad), "{bad:?}");
        }
    }
}
