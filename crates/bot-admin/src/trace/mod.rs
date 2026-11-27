//! OTLP trace exporter and redaction policy; shared instrumentation ports live in `bot-core`.
pub use bot_core::trace::*;
mod export;
mod fingerprint;
mod otlp;
mod policy;

pub use export::{Backend, Exporter, HeaderValue, Options, ProfileTrace, Stats};
pub use fingerprint::fingerprint;
pub use policy::{Ids, Redactor};
