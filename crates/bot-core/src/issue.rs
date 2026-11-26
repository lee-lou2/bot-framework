//! 오류·이슈 보고 포트(특정 백엔드에 묶이지 않는 공통 계약).
//!
//! 호출하는 쪽은 오류 사건(`IssueEvent`)을 `IssueReporter::report`에 넘긴다.
//! 모든 호출은 `Tracer`와 마찬가지로 **동기·무대기·무실패**여야 하며, 가득 차거나 내보낼 수 없으면 스스로 버린다.
//! 실제 가림 처리·샘플링·전송(Sentry Envelope 등)은 어댑터(`bot-admin`)가 맡는다.

use crate::{id::ProfileId, trace::now_nano};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

static EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

/// 이슈 심각도.
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

/// 외부 이슈 트래커로 보낼 오류 사건 하나.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueEvent {
    /// 사건 고유 ID(16바이트, 32자리 16진수 UUID v4 모양).
    pub event_id: [u8; 16],
    /// 발생 시각(UTC 나노초).
    pub timestamp_nano: u64,
    pub level: IssueLevel,
    /// 오류 분류(고정 어휘 또는 예외 종류, 예: `tool_failed`·`panic`·`log.error`).
    pub kind: String,
    /// 요약 문구(어댑터가 내보내기 전에 비밀 가림과 길이 제한을 건다).
    pub message: String,
    /// 발생 모듈·타깃(예: `bot_gateway::delivery`, `worker.run`).
    pub target: String,
    pub profile: Option<ProfileId>,
    /// 연결된 OTel 추적 ID(있으면 이슈와 추적을 잇는다).
    pub trace_id: Option<[u8; 16]>,
    /// 연결된 OTel 스팬 ID(있으면 이슈를 정확한 스팬에 잇는다).
    pub span_id: Option<[u8; 8]>,
    /// 추가 태그(키·값 모두 어댑터가 가림과 길이 제한을 건다).
    pub tags: Vec<(String, String)>,
}

/// 프로세스 내에서 겹치지 않는 UUID v4 모양의 16바이트 사건 ID를 만든다.
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
    // RFC 4122 UUID v4 비트
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

/// 오류 사건을 받는 곳. **모든 메서드는 동기·무대기·무실패**여야 한다.
pub trait IssueReporter: Send + Sync {
    fn report(&self, event: IssueEvent);
}

/// 아무 일도 하지 않는 기본 구현.
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
