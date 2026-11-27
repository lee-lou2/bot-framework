//! Spec-validating fake remote MCP server (`FakeMcp`) and wire transport (`FakeWire`) reusable over real sockets in integration tests.
//!
//! Enforces per-version requirements (`MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`, `Mcp-Param-*`,
//! `_meta` version/capabilities for `2026-07-28`; `initialize -> notifications/initialized` and
//! `Mcp-Session-Id` for legacy versions) and returns `400` on mismatch.

use crate::json::parse;
use serde_json::{Value, json};
use std::sync::Mutex;

pub(super) use super::lock;

#[derive(Clone, Debug)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

impl McpTool {
    pub fn new(name: &str, description: &str, schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
        }
    }

    fn listed(&self) -> Value {
        json!({"name": self.name, "title": "Display", "description": self.description, "inputSchema": self.schema})
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Era {
    /// Modern only (`2026-07-28`).
    Modern,
    /// Legacy only (`2025-11-25` by default); `server/discover` responds according to `probe`.
    Legacy,
}

/// How a legacy server answers an unknown `server/discover` probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Probe {
    Bare400,
    NotFound404,
    NotAllowed405,
    Rpc200,
    Modern400Legacy,
    Modern400Older,
    Modern400None,
}

#[derive(Clone, Debug)]
pub struct McpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl McpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn rpc(&self) -> String {
        parse(&self.body)
            .and_then(|v| v.get("method").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug)]
pub struct McpReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub chunks: Vec<Vec<u8>>,
}

fn reply(status: u16, headers: &[(&str, &str)], body: &str) -> McpReply {
    McpReply {
        status,
        headers: headers
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect(),
        chunks: if body.is_empty() {
            Vec::new()
        } else {
            vec![body.as_bytes().to_vec()]
        },
    }
}

fn json_reply(status: u16, body: &Value) -> McpReply {
    reply(
        status,
        &[("Content-Type", "application/json")],
        &body.to_string(),
    )
}

fn error(status: u16, id: &Value, code: i64, message: &str, data: Option<Value>) -> McpReply {
    let mut err = json!({"code": code, "message": message});
    if let Some(data) = data {
        err["data"] = data;
    }
    json_reply(status, &json!({"jsonrpc": "2.0", "id": id, "error": err}))
}

pub struct FakeMcp {
    pub era: Era,
    pub probe: Probe,
    tools: Mutex<Vec<McpTool>>,
    pub token: Option<String>,
    pub sse: bool,
    pub page: usize,
    pub session: Option<String>,
    pub delete_status: u16,
    pub echo: Option<String>,
    pub redirect: Option<(String, String)>,
    pub hang: Option<String>,
    pub fail: Option<(String, u16)>,
    pub ping: bool,
    /// Protocol version returned by legacy `initialize` (`2025-03-26` omits `MCP-Protocol-Version`).
    pub legacy: &'static str,
    pub fail_body: Option<String>,
    pub next_cursor: Option<Value>,
    pub list_changed: bool,
    pub echo_number: Option<u64>,
    requests: Mutex<Vec<McpRequest>>,
}

impl FakeMcp {
    pub fn new(era: Era, tools: Vec<McpTool>) -> Self {
        Self {
            era,
            probe: Probe::Bare400,
            tools: Mutex::new(tools),
            token: None,
            sse: false,
            page: 0,
            session: (era == Era::Legacy).then(|| "sess-1".to_owned()),
            delete_status: 200,
            echo: None,
            redirect: None,
            hang: None,
            fail: None,
            ping: false,
            legacy: "2025-11-25",
            fail_body: None,
            next_cursor: None,
            list_changed: false,
            echo_number: None,
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn set_tools(&self, tools: Vec<McpTool>) {
        *lock(&self.tools) = tools;
    }

    pub fn requests(&self) -> Vec<McpRequest> {
        lock(&self.requests).clone()
    }

    pub fn count(&self, rpc: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.rpc() == rpc || (rpc == "DELETE" && r.method == "DELETE"))
            .count()
    }

    pub fn handle(&self, request: McpRequest) -> McpReply {
        lock(&self.requests).push(request.clone());
        if let Some(token) = &self.token
            && request.header("authorization") != Some(format!("Bearer {token}").as_str())
        {
            return reply(401, &[], "");
        }
        if request.method == "DELETE" {
            return reply(self.delete_status, &[], "");
        }
        let Some(body) = parse(&request.body) else {
            return reply(400, &[], "");
        };
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let method = body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match self.era {
            Era::Modern => self.modern(&request, &body, &id, method),
            Era::Legacy => self.legacy(&request, &body, &id, method),
        }
    }

    fn page_of(&self, cursor: Option<&str>) -> Value {
        let tools = lock(&self.tools).clone();
        let size = if self.page == 0 {
            tools.len().max(1)
        } else {
            self.page
        };
        let start: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
        let end = (start + size).min(tools.len());
        let mut result = json!({"tools": tools[start.min(end)..end].iter().map(McpTool::listed).collect::<Vec<_>>()});
        if end < tools.len() {
            result["nextCursor"] = json!(end.to_string());
        }
        if cursor.is_none()
            && let Some(forced) = &self.next_cursor
        {
            result["nextCursor"] = forced.clone();
        }
        result
    }

    fn found(&self, name: &str) -> Option<McpTool> {
        lock(&self.tools).iter().find(|t| t.name == name).cloned()
    }

    fn deliver(&self, id: &Value, result: Value) -> McpReply {
        self.deliver_with(id, result, self.ping)
    }

    fn deliver_with(&self, id: &Value, result: Value, ping: bool) -> McpReply {
        let message = json!({"jsonrpc": "2.0", "id": id, "result": result});
        if !self.sse {
            return json_reply(200, &message);
        }
        let mut chunks: Vec<Vec<u8>> = vec![
            b": keep-alive\n\n".to_vec(),
            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":1}}\n\n"
                .to_vec(),
        ];
        if ping {
            chunks.push(
                b"data: {\"jsonrpc\":\"2.0\",\"id\":\"srv-1\",\"method\":\"ping\"}\n\n".to_vec(),
            );
        }
        // Split the response event across chunks to exercise cross-chunk SSE parsing.
        let event = format!("event: message\ndata: {message}\n\n").into_bytes();
        let cut = event.len() / 2;
        chunks.push(event[..cut].to_vec());
        chunks.push(event[cut..].to_vec());
        McpReply {
            status: 200,
            headers: vec![("Content-Type".into(), "text/event-stream".into())],
            chunks,
        }
    }

    fn call_result(&self, name: &str, args: &Value) -> Value {
        let mut text = format!("ok:{name}:{args}");
        if let Some(echo) = &self.echo {
            text.push(' ');
            text.push_str(echo);
        }
        let mut result = json!({"content": [{"type": "text", "text": text}], "isError": false});
        if let Some(n) = self.echo_number {
            result["structuredContent"] = json!({"n": n});
        }
        result
    }

    fn announce_change(&self, mut reply: McpReply) -> McpReply {
        if self.list_changed && self.sse && reply.chunks.len() > 1 {
            reply.chunks.insert(
                1,
                b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n"
                    .to_vec(),
            );
        }
        reply
    }

    fn special(&self, rpc: &str) -> Option<McpReply> {
        if let Some((on, to)) = &self.redirect
            && on == rpc
        {
            return Some(reply(302, &[("Location", to.as_str())], ""));
        }
        if let Some((on, status)) = &self.fail
            && on == rpc
        {
            return Some(match &self.fail_body {
                Some(body) => reply(*status, &[("Content-Type", "application/json")], body),
                None => reply(*status, &[], ""),
            });
        }
        if self.hang.as_deref() == Some(rpc) {
            return Some(McpReply {
                status: 200,
                headers: vec![("Content-Type".into(), "text/event-stream".into())],
                chunks: vec![b"HANG".to_vec()],
            });
        }
        None
    }

    fn modern(&self, request: &McpRequest, body: &Value, id: &Value, method: &str) -> McpReply {
        let params = body.get("params").cloned().unwrap_or(Value::Null);
        let meta = params.get("_meta").cloned().unwrap_or(Value::Null);
        let version = request.header("mcp-protocol-version");
        let declared = meta
            .get("io.modelcontextprotocol/protocolVersion")
            .and_then(Value::as_str);
        let mismatch = |why: &str| error(400, id, -32020, &format!("Header mismatch: {why}"), None);
        let Some(declared) = declared else {
            return error(400, id, -32602, "Invalid params", None);
        };
        let Some(version) = version else {
            return mismatch("MCP-Protocol-Version missing");
        };
        if version != declared {
            return mismatch("MCP-Protocol-Version");
        }
        if version != "2026-07-28" {
            return error(
                400,
                id,
                -32022,
                "Unsupported protocol version",
                Some(json!({"supported": ["2026-07-28"], "requested": version})),
            );
        }
        if request.header("mcp-method") != Some(method) {
            return mismatch("Mcp-Method");
        }
        if meta
            .get("io.modelcontextprotocol/clientCapabilities")
            .is_none()
        {
            return error(400, id, -32602, "Invalid params", None);
        }
        if let Some(special) = self.special(method) {
            return special;
        }
        match method {
            "server/discover" => self.deliver(
                id,
                json!({"resultType": "complete", "supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}},
                       "ttlMs": 0, "cacheScope": "private"}),
            ),
            "tools/list" => {
                let mut page = self.page_of(params.get("cursor").and_then(Value::as_str));
                page["resultType"] = json!("complete");
                page["ttlMs"] = json!(0);
                page["cacheScope"] = json!("private");
                self.announce_change(self.deliver(id, page))
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                if request.header("mcp-name") != Some(name) {
                    return mismatch("Mcp-Name");
                }
                let Some(tool) = self.found(name) else {
                    return error(200, id, -32602, "Unknown tool", None);
                };
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                if let Some(bad) = self.param_mismatch(request, &tool, &args) {
                    return mismatch(&bad);
                }
                let mut result = self.call_result(name, &args);
                result["resultType"] = json!("complete");
                self.deliver(id, result)
            }
            _ => error(404, id, -32601, "Method not found", None),
        }
    }

    fn param_mismatch(&self, request: &McpRequest, tool: &McpTool, args: &Value) -> Option<String> {
        let props = tool.schema.get("properties")?.as_object()?;
        for (name, schema) in props {
            let Some(header) = schema.get("x-mcp-header").and_then(Value::as_str) else {
                continue;
            };
            let want = match args.get(name) {
                Some(Value::String(text)) => Some(text.clone()),
                Some(Value::Bool(flag)) => Some(flag.to_string()),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            };
            let got = request.header(&format!("Mcp-Param-{header}"));
            match (want, got) {
                (None, None) => {}
                (Some(want), Some(got)) if got == want || got.starts_with("=?base64?") => {}
                _ => return Some(format!("Mcp-Param-{header}")),
            }
        }
        None
    }

    fn legacy(&self, request: &McpRequest, body: &Value, id: &Value, method: &str) -> McpReply {
        match method {
            "initialize" => {
                let params = body.get("params").cloned().unwrap_or(Value::Null);
                if params.get("protocolVersion").and_then(Value::as_str) != Some("2025-11-25") {
                    return error(400, id, -32602, "Unsupported protocol version", None);
                }
                let result = json!({"protocolVersion": self.legacy, "capabilities": {"tools": {}},
                                    "serverInfo": {"name": "fake", "version": "1"}});
                let mut out = self.deliver_with(id, result, false);
                if let Some(session) = &self.session {
                    out.headers.push(("Mcp-Session-Id".into(), session.clone()));
                }
                out
            }
            "server/discover" => self.discover_reply(id),
            _ => {
                if self.legacy != "2025-03-26"
                    && request.header("mcp-protocol-version") != Some(self.legacy)
                {
                    return reply(400, &[], "");
                }
                if let Some(session) = &self.session
                    && request.header("mcp-session-id") != Some(session.as_str())
                {
                    return reply(400, &[], "");
                }
                if method == "notifications/initialized" {
                    return reply(202, &[], "");
                }
                if method.is_empty() {
                    // Client reply to a server-initiated request.
                    return reply(202, &[], "");
                }
                if let Some(special) = self.special(method) {
                    return special;
                }
                match method {
                    "tools/list" => {
                        let params = body.get("params").cloned().unwrap_or(Value::Null);
                        let page = self.page_of(params.get("cursor").and_then(Value::as_str));
                        self.announce_change(self.deliver_with(id, page, false))
                    }
                    "tools/call" => {
                        let params = body.get("params").cloned().unwrap_or(Value::Null);
                        let name = params
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if self.found(name).is_none() {
                            return error(200, id, -32602, "Unknown tool", None);
                        }
                        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                        self.deliver(id, self.call_result(name, &args))
                    }
                    _ => error(200, id, -32601, "Method not found", None),
                }
            }
        }
    }

    fn discover_reply(&self, id: &Value) -> McpReply {
        match self.probe {
            Probe::Bare400 => reply(400, &[], ""),
            Probe::NotFound404 => reply(404, &[], "not found"),
            Probe::NotAllowed405 => reply(405, &[], ""),
            Probe::Rpc200 => error(200, id, -32601, "Method not found", None),
            Probe::Modern400Legacy => error(
                400,
                id,
                -32022,
                "Unsupported protocol version",
                Some(json!({"supported": ["2025-11-25"]})),
            ),
            Probe::Modern400Older => error(
                400,
                id,
                -32022,
                "Unsupported protocol version",
                Some(json!({"supported": ["2025-06-18"]})),
            ),
            Probe::Modern400None => error(
                400,
                id,
                -32022,
                "Unsupported protocol version",
                Some(json!({"supported": ["1999-01-01"]})),
            ),
        }
    }
}

mod fake_wire;

pub use fake_wire::FakeWire;
