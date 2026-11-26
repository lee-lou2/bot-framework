//! OTLP 로그 내보내기(`/v1/logs`). 구조화 로그를 OTLP/HTTP(S) protobuf로 수집기에 보낸다.
//! 모든 호출은 동기·무대기·무실패이며, 알려진 비밀 값과 토큰 모양을 내보내기 직전에 가린다.

mod export;
mod fingerprint;
mod otlp;

pub use export::{LogConfig, LogEntry, LogExporter, LogLevel};
pub use fingerprint::fingerprint;
