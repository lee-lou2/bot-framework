//! OTLP/HTTP log exporter (`/v1/logs`) with non-blocking queuing and secret redaction.

mod export;
mod fingerprint;
mod otlp;

pub use export::{LogConfig, LogEntry, LogExporter, LogLevel};
pub use fingerprint::fingerprint;
