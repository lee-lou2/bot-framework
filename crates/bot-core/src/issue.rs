//! Backend-agnostic error and issue reporting port (synchronous, non-blocking, infallible).

use crate::{id::ProfileId, trace::now_nano};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

static EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueLevel {
    Error,
    Fatal,
}

impl IssueLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Fatal => "fatal",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueEvent {
    pub event_id: [u8; 16],
    pub timestamp_nano: u64,
    pub level: IssueLevel,
    /// Fixed-vocabulary error class or exception kind (e.g. `tool_failed`, `panic`, `log.error`).
    pub kind: String,
    /// Redacted and length-capped by the exporter adapter before transmission.
    pub message: String,
    pub target: String,
    pub profile: Option<ProfileId>,
    pub trace_id: Option<[u8; 16]>,
    pub span_id: Option<[u8; 8]>,
    /// Redacted and length-capped by the exporter adapter before transmission.
    pub tags: Vec<(String, String)>,
}

pub fn new_event_id() -> [u8; 16] {
    let seq = EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut hash = Sha256::new();
    hash.update(b"bot-issue-event-v1\0");
    hash.update(std::process::id().to_be_bytes());
    hash.update(now_nano().to_be_bytes());
    hash.update(seq.to_be_bytes());
    let digest = hash.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    // RFC 4122 UUID v4 bits
    id[6] = (id[6] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;
    id
}

impl IssueEvent {
    pub fn new(
        level: IssueLevel,
        kind: impl Into<String>,
        message: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            event_id: new_event_id(),
            timestamp_nano: now_nano(),
            level,
            kind: kind.into(),
            message: message.into(),
            target: target.into(),
            profile: None,
            trace_id: None,
            span_id: None,
            tags: Vec::new(),
        }
    }

    pub fn profile(mut self, profile: ProfileId) -> Self {
        self.profile = Some(profile);
        self
    }

    pub fn trace_id(mut self, trace_id: [u8; 16]) -> Self {
        self.trace_id = Some(trace_id);
        self
    }

    pub fn span_id(mut self, span_id: [u8; 8]) -> Self {
        self.span_id = Some(span_id);
        self
    }

    pub fn tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.tags.push((key.into(), value.into()));
        self
    }
}

/// All methods must be synchronous, non-blocking, and infallible (drop on overflow); redaction and transport live in the adapter.
pub trait IssueReporter: Send + Sync {
    fn report(&self, event: IssueEvent);
}

pub struct NoopIssueReporter;

impl IssueReporter for NoopIssueReporter {
    fn report(&self, _: IssueEvent) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn event_ids_are_unique_and_rfc4122_v4_shaped() {
        let mut seen = HashSet::new();
        for _ in 0..500 {
            let id = new_event_id();
            assert_eq!(id[6] >> 4, 4);
            assert_eq!(id[8] >> 6, 2);
            assert!(seen.insert(id));
        }
    }

    #[test]
    fn builder_fills_optional_fields_and_level_labels() {
        let ev = IssueEvent::new(
            IssueLevel::Fatal,
            "panic",
            "unexpected state",
            "bot_cli::serve",
        )
        .profile(ProfileId("personal".into()))
        .trace_id([7; 16])
        .span_id([9; 8])
        .tag("component", "worker");
        assert_eq!(ev.level.as_str(), "fatal");
        assert_eq!(IssueLevel::Error.as_str(), "error");
        assert_eq!(ev.profile, Some(ProfileId("personal".into())));
        assert_eq!(ev.trace_id, Some([7; 16]));
        assert_eq!(ev.span_id, Some([9; 8]));
        assert_eq!(ev.tags, vec![("component".into(), "worker".into())]);
    }
}
