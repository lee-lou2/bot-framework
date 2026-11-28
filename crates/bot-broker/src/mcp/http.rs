//! MCP Streamable HTTP client (one session per call across modern and legacy protocol versions).
//!
//! Probes with read-only `server/discover` for `2026-07-28`, falling back to legacy `initialize`
//! (`2025-11-25`, `2025-06-18`, `2025-03-26`) only on spec-defined compatibility responses; never
//! probes or retries `tools/call`. Declares empty `clientCapabilities` and opens no GET stream.

mod message;

use super::{
    MAX_FRAME, MAX_PAGES, MAX_SKIPPED, McpError,
    remote::Remote,
    sse::Events,
    wire::{RemoteWire, WireFail, WireMethod, WireRequest, WireResponse, header_value},
};
use crate::{json::parse, model::ChunkSource, text::json_quote};
use message::{
    Answer, Message, error_message, finish, holds, holds_bytes, listed, media_type, read_all,
    response_message, session_ok,
};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::time::timeout;

pub const MODERN: &str = "2026-07-28";
pub const LEGACY: &str = "2025-11-25";
pub const LEGACY_VERSIONS: [&str; 3] = [LEGACY, "2025-06-18", "2025-03-26"];
/// Legacy version predating the `MCP-Protocol-Version` request header (introduced in `2025-06-18`).
const NO_VERSION_HEADER: &str = "2025-03-26";

const CLIENT_INFO: &str = r#"{"name":"bot-broker","version":"0.1.0"}"#;
const MODERN_META: &str = r#""io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"bot-broker","version":"0.1.0"},"io.modelcontextprotocol/protocolVersion":"2026-07-28""#;
const MAX_STREAM: usize = 8 << 20;
const AUX_LIMIT: Duration = Duration::from_secs(5);
const MAX_LISTED: usize = 4096;
const MAX_CURSOR: usize = 4096;
const LIST_CHANGED: &str = "notifications/tools/list_changed";

#[derive(Debug)]
enum Era {
    Probe,
    Modern,
    Legacy {
        session: Option<String>,
        version: &'static str,
    },
}

pub struct Http {
    wire: Arc<dyn RemoteWire>,
    remote: Remote,
    bearer: Option<String>,
    allow_private: bool,
    era: Era,
    next: u64,
    changed: AtomicBool,
    auth_challenge: AtomicBool,
    /// Per-call transport timeout (`limit + AUX_LIMIT`) set only during `tools/call`.
    wire_timeout: Option<Duration>,
}

fn offers_legacy(versions: &[&str]) -> bool {
    versions.iter().any(|v| LEGACY_VERSIONS.contains(v))
}

impl Http {
    pub fn new(
        wire: Arc<dyn RemoteWire>,
        remote: Remote,
        bearer: Option<String>,
        allow_private: bool,
    ) -> Self {
        Self {
            wire,
            remote,
            bearer,
            allow_private,
            era: Era::Probe,
            next: 1,
            changed: AtomicBool::new(false),
            auth_challenge: AtomicBool::new(false),
            wire_timeout: None,
        }
    }

    /// True if the last HTTP 401 response included a `WWW-Authenticate` header (value is never retained).
    pub fn auth_challenge(&self) -> bool {
        self.auth_challenge.load(Ordering::SeqCst)
    }

    /// Rejects any server-supplied bytes echoing our bearer credential.
    fn guard(&self, raw: &[u8]) -> Result<(), McpError> {
        if holds_bytes(raw, self.bearer.as_deref()) {
            Err(McpError::Exposed)
        } else {
            Ok(())
        }
    }

    pub fn version(&self) -> Option<&'static str> {
        match self.era {
            Era::Probe => None,
            Era::Modern => Some(MODERN),
            Era::Legacy { version, .. } => Some(version),
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    fn plain_headers(&self) -> Vec<(String, String, bool)> {
        let mut headers = vec![
            ("Content-Type".into(), "application/json".into(), false),
            (
                "Accept".into(),
                "application/json, text/event-stream".into(),
                false,
            ),
            ("User-Agent".into(), "bot-broker/0.1".into(), false),
        ];
        if let Some(token) = &self.bearer {
            headers.push(("Authorization".into(), format!("Bearer {token}"), true));
        }
        headers
    }

    fn modern_headers(
        &self,
        method: &str,
        name: Option<&str>,
        params: Vec<(String, String)>,
    ) -> Vec<(String, String, bool)> {
        let mut headers = self.plain_headers();
        headers.push(("MCP-Protocol-Version".into(), MODERN.into(), false));
        headers.push(("Mcp-Method".into(), method.into(), false));
        if let Some(name) = name {
            headers.push(("Mcp-Name".into(), super::header::encode_value(name), false));
        }
        headers.extend(params.into_iter().map(|(n, v)| (n, v, false)));
        headers
    }

    fn legacy_headers(&self) -> Vec<(String, String, bool)> {
        let mut headers = self.plain_headers();
        if let Era::Legacy { session, version } = &self.era {
            if *version != NO_VERSION_HEADER {
                headers.push(("MCP-Protocol-Version".into(), (*version).into(), false));
            }
            if let Some(session) = session {
                headers.push(("Mcp-Session-Id".into(), session.clone(), false));
            }
        }
        headers
    }

    /// Builds a JSON-RPC request body from raw object-member fragments `fields` and `meta`.
    fn body(&self, id: u64, method: &str, fields: &str, meta: &str) -> String {
        let mut entries: Vec<&str> = Vec::new();
        if !matches!(self.era, Era::Legacy { .. }) {
            entries.push(MODERN_META);
        }
        if !meta.is_empty() {
            entries.push(meta);
        }
        let mut members = fields.to_owned();
        if !entries.is_empty() {
            if !members.is_empty() {
                members.push(',');
            }
            members.push_str(&format!("\"_meta\":{{{}}}", entries.join(",")));
        }
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":{},\"params\":{{{members}}}}}",
            json_quote(method)
        )
    }

    async fn send(
        &self,
        method: WireMethod,
        headers: Vec<(String, String, bool)>,
        body: String,
    ) -> Result<WireResponse, McpError> {
        let request = WireRequest {
            method,
            tls: self.remote.tls,
            host: self.remote.host.clone(),
            port: self.remote.port,
            path: self.remote.path.clone(),
            headers,
            body: body.into_bytes(),
            allow_private: self.allow_private,
            timeout: self.wire_timeout,
        };
        self.wire.send(request).await.map_err(|fail| match fail {
            WireFail::Unreached => McpError::Unreached,
            WireFail::Lost => McpError::Io,
            WireFail::Stale => McpError::Stale,
        })
    }

    async fn exchange(
        &self,
        id: u64,
        headers: Vec<(String, String, bool)>,
        body: String,
    ) -> Result<Answer, McpError> {
        let WireResponse {
            status,
            headers: reply,
            body: mut stream,
        } = self.send(WireMethod::Post, headers, body).await?;
        let session = header_value(&reply, "mcp-session-id").map(str::to_owned);
        if let Some(session) = &session {
            self.guard(session.as_bytes())?;
        }
        if !(200..300).contains(&status) {
            self.auth_challenge.store(
                status == 401 && header_value(&reply, "www-authenticate").is_some(),
                Ordering::SeqCst,
            );
            let message = error_message(&mut *stream, id, self.bearer.as_deref()).await?;
            return Ok(Answer {
                status,
                session,
                message,
            });
        }
        self.auth_challenge.store(false, Ordering::SeqCst);
        if matches!(status, 202 | 204) {
            return Ok(Answer {
                status,
                session,
                message: Message::Empty,
            });
        }
        let kind = header_value(&reply, "content-type").map(media_type);
        let message = match kind.as_deref() {
            Some("application/json") => {
                let bytes = read_all(&mut *stream, MAX_FRAME).await?;
                self.guard(&bytes)?;
                let text = std::str::from_utf8(&bytes).map_err(|_| McpError::Protocol)?;
                let value = parse(text).ok_or(McpError::Protocol)?;
                if holds(&value, self.bearer.as_deref()) {
                    return Err(McpError::Exposed);
                }
                response_message(&value, id)?.ok_or(McpError::Protocol)?
            }
            Some("text/event-stream") => self.stream(&mut *stream, id).await?,
            _ => return Err(McpError::Protocol),
        };
        Ok(Answer {
            status,
            session,
            message,
        })
    }

    async fn stream(&self, body: &mut dyn ChunkSource, id: u64) -> Result<Message, McpError> {
        let mut events = Events::new(MAX_FRAME);
        let (mut total, mut seen) = (0usize, 0usize);
        loop {
            let chunk = match body.next().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => return Err(McpError::Closed),
                Err(_) => return Err(McpError::Io),
            };
            total += chunk.len();
            if total > MAX_STREAM {
                return Err(McpError::Protocol);
            }
            for data in events.push(&chunk).map_err(|_| McpError::Protocol)? {
                seen += 1;
                if seen > MAX_SKIPPED {
                    return Err(McpError::Protocol);
                }
                // Guard raw and decoded values before any reply (such as legacy request refusal) can echo IDs.
                self.guard(data.as_bytes())?;
                let value = parse(&data).ok_or(McpError::Protocol)?;
                if holds(&value, self.bearer.as_deref()) {
                    return Err(McpError::Exposed);
                }
                let obj = value.as_object().ok_or(McpError::Protocol)?;
                if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                    return Err(McpError::Protocol);
                }
                if let Some(method) = obj.get("method").and_then(Value::as_str) {
                    match (obj.get("id"), &self.era) {
                        (None, _) => {
                            if method == LIST_CHANGED {
                                self.changed.store(true, Ordering::SeqCst);
                            }
                        }
                        (Some(request), Era::Legacy { .. }) => self.refuse(request, method).await,
                        (Some(_), _) => return Err(McpError::Protocol),
                    }
                    continue;
                }
                if let Some(message) = response_message(&value, id)? {
                    return Ok(message);
                }
            }
        }
    }

    /// Replies to legacy server-initiated requests (`ping` -> `{}`, others -> `-32601`).
    async fn refuse(&self, request: &Value, method: &str) {
        let id = request.to_string();
        let body = if method == "ping" {
            format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}")
        } else {
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32601,\"message\":\"method not supported\"}}}}"
            )
        };
        let _ = timeout(
            AUX_LIMIT,
            self.send(WireMethod::Post, self.legacy_headers(), body),
        )
        .await;
    }

    pub async fn connect(&mut self, limit: Duration) -> Result<(), McpError> {
        timeout(limit, self.negotiate())
            .await
            .map_err(|_| McpError::Timeout)?
    }

    async fn negotiate(&mut self) -> Result<(), McpError> {
        let id = self.take_id();
        let body = self.body(id, "server/discover", "", "");
        let headers = self.modern_headers("server/discover", None, Vec::new());
        let Answer {
            status, message, ..
        } = self.exchange(id, headers, body).await?;
        match (status, message) {
            (200..=299, Message::Result(result)) => {
                let versions = listed(result.get("supportedVersions"));
                if versions.contains(&MODERN) {
                    self.era = Era::Modern;
                    Ok(())
                } else if offers_legacy(&versions) {
                    self.initialize().await
                } else {
                    Err(McpError::Version)
                }
            }
            // Modern unsupported-version error (-32022): fall back only if a known legacy version is advertised.
            (400, Message::Error { code: -32022, data }) => {
                let versions = listed(data.as_ref().and_then(|d| d.get("supported")));
                if versions.contains(&MODERN) {
                    Err(McpError::Protocol)
                } else if offers_legacy(&versions) {
                    self.initialize().await
                } else {
                    Err(McpError::Version)
                }
            }
            // Modern protocol errors (-32021..-32020) must not fall back to legacy.
            (400, Message::Error { code, .. }) if (-32021..=-32020).contains(&code) => {
                Err(McpError::Status {
                    status,
                    code: Some(code),
                })
            }
            (200..=299, Message::Empty) => Err(McpError::Protocol),
            // Legacy compatibility signal: non-modern 400/404/405 or 2xx JSON-RPC error on unknown `server/discover`.
            (400 | 404 | 405, Message::Empty | Message::Error { .. })
            | (200..=299, Message::Error { .. }) => self.initialize().await,
            (status, Message::Error { code, .. }) => Err(McpError::Status {
                status,
                code: Some(code),
            }),
            (status, _) => Err(McpError::Status { status, code: None }),
        }
    }

    async fn initialize(&mut self) -> Result<(), McpError> {
        let id = self.take_id();
        let body = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"initialize\",\"params\":{{\"protocolVersion\":{},\"capabilities\":{{}},\"clientInfo\":{CLIENT_INFO}}}}}",
            json_quote(LEGACY)
        );
        let answer = self.exchange(id, self.plain_headers(), body).await?;
        let session = answer.session.clone();
        let result = finish(answer)?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .and_then(|given| {
                LEGACY_VERSIONS
                    .iter()
                    .copied()
                    .find(|known| *known == given)
            })
            .ok_or(McpError::Version)?;
        if session.as_deref().is_some_and(|text| !session_ok(text)) {
            return Err(McpError::Protocol);
        }
        self.era = Era::Legacy { session, version };
        let response = self
            .send(
                WireMethod::Post,
                self.legacy_headers(),
                "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}".into(),
            )
            .await?;
        if (200..300).contains(&response.status) {
            Ok(())
        } else {
            Err(McpError::Status {
                status: response.status,
                code: None,
            })
        }
    }

    async fn request(
        &mut self,
        method: &str,
        fields: &str,
        meta: &str,
        name: Option<&str>,
        params: Vec<(String, String)>,
    ) -> Result<Value, McpError> {
        let id = self.take_id();
        let body = self.body(id, method, fields, meta);
        let headers = match self.era {
            Era::Legacy { .. } => self.legacy_headers(),
            _ => self.modern_headers(method, name, params),
        };
        finish(self.exchange(id, headers, body).await?)
    }

    /// Fetches all paginated tool definitions; fails on `list_changed`, cursor loops, or exceeded caps.
    pub async fn tools(&mut self, limit: Duration) -> Result<Vec<Value>, McpError> {
        self.changed.store(false, Ordering::SeqCst);
        let mut all = Vec::new();
        let (mut cursor, mut used): (Option<String>, Vec<String>) = (None, Vec::new());
        for _ in 0..MAX_PAGES {
            let fields = cursor
                .as_deref()
                .map_or_else(String::new, |c| format!("\"cursor\":{}", json_quote(c)));
            let result = timeout(
                limit,
                self.request("tools/list", &fields, "", None, Vec::new()),
            )
            .await
            .map_err(|_| McpError::Timeout)??;
            if self.changed.swap(false, Ordering::SeqCst) {
                return Err(McpError::Changed);
            }
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or(McpError::Protocol)?;
            if all.len() + page.len() > MAX_LISTED {
                return Err(McpError::Protocol);
            }
            all.extend(page.iter().cloned());
            match result.get("nextCursor") {
                None => return Ok(all),
                Some(Value::String(next))
                    if !next.is_empty() && next.len() <= MAX_CURSOR && !used.contains(next) =>
                {
                    used.push(next.clone());
                    cursor = Some(next.clone());
                }
                Some(_) => return Err(McpError::Protocol),
            }
        }
        Err(McpError::Protocol)
    }

    /// Finds a unique tool definition by name; returns `Protocol` if duplicated.
    pub async fn find_tool(
        &mut self,
        name: &str,
        limit: Duration,
    ) -> Result<Option<Value>, McpError> {
        let mut found = None;
        for tool in self.tools(limit).await? {
            if tool.get("name").and_then(Value::as_str) == Some(name) {
                if found.is_some() {
                    return Err(McpError::Protocol);
                }
                found = Some(tool);
            }
        }
        Ok(found)
    }

    /// Sends one `tools/call` without retries; pads the wire timeout so `limit` expires first.
    pub async fn call(
        &mut self,
        name: &str,
        arguments: &str,
        meta: &str,
        params: Vec<(String, String)>,
        limit: Duration,
    ) -> Result<Value, McpError> {
        let fields = format!("\"name\":{},\"arguments\":{arguments}", json_quote(name));
        // Keep transport timeout above `limit` so long calls report `Timeout` instead of idle wire cutoff.
        self.wire_timeout = Some(limit + AUX_LIMIT);
        let result = timeout(
            limit,
            self.request("tools/call", &fields, meta, Some(name), params),
        )
        .await
        .map_err(|_| McpError::Timeout);
        self.wire_timeout = None;
        result?
    }

    pub async fn close(&mut self) {
        if let Era::Legacy {
            session: Some(_), ..
        } = &self.era
        {
            let _ = timeout(
                AUX_LIMIT,
                self.send(WireMethod::Delete, self.legacy_headers(), String::new()),
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests;
