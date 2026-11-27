//! Outbound HTTP transport port for remote MCP servers with per-connection egress vetting.
//!
//! Implementations must enforce:
//! - Fresh DNS resolution per connection vetted via `egress::vet_resolved` (no connection pooling across
//!   requests); IP literals undergo the same check except loopback literals with `tls == false`.
//! - No redirects (return 3xx as-is), retries, proxies, or automatic decompression.
//! - Secret header values (`Authorization`) must never appear in logs or errors.
//! - Pre-connect failures return `Unreached`; any failure after starting to write the request returns `Lost`.

use crate::model::ChunkSource;
use std::{future::Future, pin::Pin};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireMethod {
    Post,
    Delete,
}

pub struct WireRequest {
    pub method: WireMethod,
    /// `true` for HTTPS; `false` is permitted only for loopback IP literals.
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
    /// `(name, value, is_secret)`; secret values are omitted from diagnostics.
    pub headers: Vec<(String, String, bool)>,
    pub body: Vec<u8>,
    /// True only when the operator explicitly enabled `RunnerConfig::allow_private_egress`.
    pub allow_private: bool,
    /// Post-connect response deadline for long `tools/call` requests; `None` uses the transport idle default.
    pub timeout: Option<std::time::Duration>,
}

/// Logs header names only so credentials never leak through `Debug`.
impl std::fmt::Debug for WireRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(n, _, _)| n.as_str()).collect();
        f.debug_struct("WireRequest")
            .field("method", &self.method)
            .field("tls", &self.tls)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("headers", &names)
            .field("body_len", &self.body.len())
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

pub struct WireResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn ChunkSource>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireFail {
    /// Failed before or during connection setup; server was never reached.
    Unreached,
    /// Failed after request transmission began; delivery outcome is unknown.
    Lost,
    /// Pre-send revision check failed or could not be confirmed; nothing was sent.
    Stale,
}

pub type WireReply<'a> = Pin<Box<dyn Future<Output = Result<WireResponse, WireFail>> + Send + 'a>>;

pub trait RemoteWire: Send + Sync {
    fn send(&self, request: WireRequest) -> WireReply<'_>;
}

pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}
