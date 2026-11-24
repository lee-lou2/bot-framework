//! OTLP 내보내기와 정책. 공통 계측 포트는 bot-core 소유다.
pub use bot_core::trace::*;
mod export;
mod fingerprint;
mod http;
mod otlp;
mod policy;

pub use export::{Backend, Exporter, HeaderValue, Options, ProfileTrace, Stats};
pub use fingerprint::fingerprint;
pub use policy::{Ids, Redactor};
