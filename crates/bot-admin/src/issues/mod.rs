//! 이슈 트래커 내보내기(Sentry Envelope v7 프로토콜 호환: Sentry·GlitchTip·Bugsink).
//! 공통 보고 계약(`bot_core::issue::IssueReporter`)을 구현한다.

mod export;
mod fingerprint;
mod sentry;

pub use bot_core::issue::{IssueEvent, IssueLevel, IssueReporter, NoopIssueReporter};
pub use export::{IssueConfig, IssueExporter, IssueKind, IssueOptions};
pub use fingerprint::fingerprint;
