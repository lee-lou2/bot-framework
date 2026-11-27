//! Stdio MCP JSON-RPC 2.0 client and result validation (remote HTTP lives in `http`).
//!
//! One session per call: `initialize` (version check) -> `tools/list` -> `tools/call`.
//! Server-initiated requests are rejected and notifications are ignored; outputs are size-capped and schema-checked.

use crate::{
    canon::canonicalize,
    json::parse,
    lines::read_line,
    sandbox::{BoxRead, BoxWrite, Pipe},
    text::json_quote,
};
use serde_json::{Map, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    time::timeout,
};

pub mod header;
pub mod http;
pub mod remote;
pub(crate) mod sse;
pub mod wire;

/// Supported MCP protocol versions; servers negotiating anything else are rejected.
pub const SUPPORTED: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// Maximum byte length of a single JSON-RPC line from the server.
pub const MAX_FRAME: usize = 1 << 20;
const MAX_SKIPPED: usize = 256;
const MAX_PAGES: usize = 16;

#[derive(Debug, Eq, PartialEq)]
pub enum McpError {
    Io,
    Timeout,
    Closed,
    Protocol,
    Version,
    /// JSON-RPC error response from the server (code only).
    Server(i64),
    /// (HTTP) Failed before connecting; the request never reached the server.
    Unreached,
    /// (HTTP) Non-2xx response; `code` is populated only when the body matches the request's JSON-RPC error (never used to infer non-execution).
    Status {
        status: u16,
        code: Option<i64>,
    },
    /// (HTTP) Outbound credentials appeared in the server response; output is discarded.
    Exposed,
    /// (HTTP) Server signaled a tool list change mid-pagination.
    Changed,
    /// (HTTP) Process revision became stale before sending; nothing was transmitted.
    Stale,
}

pub struct Session {
    read: BufReader<BoxRead>,
    write: BoxWrite,
    next: u64,
}

impl Session {
    pub fn new(pipe: Pipe) -> Self {
        Self {
            read: BufReader::new(pipe.read),
            write: pipe.write,
            next: 1,
        }
    }

    async fn send_frame(&mut self, mut text: String) -> Result<(), McpError> {
        text.push('\n');
        self.write
            .write_all(text.as_bytes())
            .await
            .map_err(|_| McpError::Io)?;
        self.write.flush().await.map_err(|_| McpError::Io)
    }

    async fn send(&mut self, method: &str, params: &str) -> Result<u64, McpError> {
        let id = self.next;
        self.next += 1;
        let frame = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":{},\"params\":{params}}}",
            json_quote(method)
        );
        self.send_frame(frame).await?;
        Ok(id)
    }

    async fn frame(&mut self) -> Result<Value, McpError> {
        let line = match read_line(&mut self.read, MAX_FRAME).await {
            Ok(Some(line)) => line,
            Ok(None) => return Err(McpError::Closed),
            Err(_) => return Err(McpError::Io),
        };
        let text = std::str::from_utf8(&line).map_err(|_| McpError::Protocol)?;
        parse(text).ok_or(McpError::Protocol)
    }

    async fn reject(&mut self, id: &Value) -> Result<(), McpError> {
        let id = serde_json::to_string(id).map_err(|_| McpError::Protocol)?;
        let frame = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32601,\"message\":\"method not supported\"}}}}"
        );
        self.send_frame(frame).await
    }

    async fn wait(&mut self, id: u64) -> Result<Value, McpError> {
        for _ in 0..MAX_SKIPPED {
            let frame = self.frame().await?;
            let obj = frame.as_object().ok_or(McpError::Protocol)?;
            if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                return Err(McpError::Protocol);
            }
            if obj.get("method").and_then(Value::as_str).is_some() {
                if let Some(request_id) = obj.get("id") {
                    self.reject(request_id).await?;
                }
                continue;
            }
            if obj.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = obj.get("error") {
                return Err(McpError::Server(
                    error.get("code").and_then(Value::as_i64).unwrap_or(0),
                ));
            }
            return match obj.get("result") {
                Some(result @ Value::Object(_)) => Ok(result.clone()),
                _ => Err(McpError::Protocol),
            };
        }
        Err(McpError::Protocol)
    }

    pub async fn await_result(&mut self, id: u64, limit: Duration) -> Result<Value, McpError> {
        timeout(limit, self.wait(id))
            .await
            .map_err(|_| McpError::Timeout)?
    }

    async fn request(
        &mut self,
        method: &str,
        params: &str,
        limit: Duration,
    ) -> Result<Value, McpError> {
        let id = self.send(method, params).await?;
        self.await_result(id, limit).await
    }

    pub async fn initialize(&mut self, limit: Duration) -> Result<(), McpError> {
        let params = format!(
            "{{\"protocolVersion\":{},\"capabilities\":{{}},\"clientInfo\":{{\"name\":\"bot-broker\",\"version\":\"0.1.0\"}}}}",
            json_quote(SUPPORTED[0])
        );
        let result = self.request("initialize", &params, limit).await?;
        match result.get("protocolVersion").and_then(Value::as_str) {
            Some(version) if SUPPORTED.contains(&version) => {}
            _ => return Err(McpError::Version),
        }
        self.send_frame("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}".into())
            .await
    }

    /// Finds tool `name` across paginated `tools/list`; returns an error if duplicated.
    pub async fn find_tool(
        &mut self,
        name: &str,
        limit: Duration,
    ) -> Result<Option<Value>, McpError> {
        let mut found: Option<Value> = None;
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => format!("{{\"cursor\":{}}}", json_quote(cursor)),
                None => "{}".to_owned(),
            };
            let result = self.request("tools/list", &params, limit).await?;
            let tools = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or(McpError::Protocol)?;
            for tool in tools {
                if tool.get("name").and_then(Value::as_str) == Some(name) {
                    if found.is_some() {
                        return Err(McpError::Protocol);
                    }
                    found = Some(tool.clone());
                }
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
                _ => return Ok(found),
            }
        }
        Err(McpError::Protocol)
    }

    /// Sends `tools/call` with canonical `arguments` and `_meta` JSON; failure after write leaves delivery uncertain.
    pub async fn send_call(
        &mut self,
        name: &str,
        arguments: &str,
        meta: &str,
    ) -> Result<u64, McpError> {
        let params = format!(
            "{{\"name\":{},\"arguments\":{arguments},\"_meta\":{meta}}}",
            json_quote(name)
        );
        self.send("tools/call", &params).await
    }
}

/// Result `_meta` key for tool-requested one-shot follow-up wakes.
pub const WAKE_KEY: &str = "bot/wake";
/// Result `_meta` key signaling that a direct-trigger tool declined the event (`service::pass`).
pub const PASS_KEY: &str = "bot/pass";
/// Sentinel emitted in `_meta` when `bot/pass` arrives with extra `_meta` keys so the broker rejects it as `invalid_pass`.
pub const EXTRA_KEY: &str = "bot/extra";

#[derive(Debug, Eq, PartialEq)]
pub struct Output {
    pub is_error: bool,
    pub texts: Vec<String>,
    pub structured: Option<Value>,
    /// Top-level `_meta["bot/wake"]`, stripped before forwarding to the model and converted to a schedule.
    pub wake: Option<Value>,
    /// Top-level `_meta["bot/pass"]`, stripped before forwarding to the model and validated in call context.
    pub pass: Option<Value>,
    /// True when `_meta` held keys other than `bot/wake` / `bot/pass`; emitted as `EXTRA_KEY` only alongside `pass`.
    pub meta_extra: bool,
}

/// Accepts only `{content: [{type: "text", text}], isError?, structuredContent?}` and rejects other shapes as `result_shape`.
pub fn parse_result(value: &Value) -> Result<Output, &'static str> {
    const SHAPE: &str = "result_shape";
    let obj = value.as_object().ok_or(SHAPE)?;
    let is_error = match obj.get("isError") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(SHAPE),
    };
    let mut texts = Vec::new();
    for item in obj.get("content").and_then(Value::as_array).ok_or(SHAPE)? {
        let item = item.as_object().ok_or(SHAPE)?;
        if item.get("type").and_then(Value::as_str) != Some("text") {
            return Err(SHAPE);
        }
        texts.push(
            item.get("text")
                .and_then(Value::as_str)
                .ok_or(SHAPE)?
                .to_owned(),
        );
    }
    Ok(Output {
        is_error,
        texts,
        structured: obj.get("structuredContent").cloned(),
        wake: obj
            .get("_meta")
            .and_then(Value::as_object)
            .and_then(|meta| meta.get(WAKE_KEY))
            .cloned(),
        pass: obj
            .get("_meta")
            .and_then(Value::as_object)
            .and_then(|meta| meta.get(PASS_KEY))
            .cloned(),
        meta_extra: obj
            .get("_meta")
            .and_then(Value::as_object)
            .is_some_and(|meta| meta.keys().any(|key| key != WAKE_KEY && key != PASS_KEY)),
    })
}

/// Canonicalizes a validated output for the model, dropping unknown fields.
pub fn payload(output: &Output) -> Option<String> {
    let mut map = Map::new();
    map.insert(
        "content".into(),
        Value::Array(
            output
                .texts
                .iter()
                .map(|text| {
                    let mut item = Map::new();
                    item.insert("type".into(), "text".into());
                    item.insert("text".into(), text.as_str().into());
                    Value::Object(item)
                })
                .collect(),
        ),
    );
    map.insert("isError".into(), output.is_error.into());
    if let Some(structured) = &output.structured {
        map.insert("structuredContent".into(), structured.clone());
    }
    if output.wake.is_some() || output.pass.is_some() {
        let mut meta = Map::new();
        if let Some(wake) = &output.wake {
            meta.insert(WAKE_KEY.into(), wake.clone());
        }
        if let Some(pass) = &output.pass {
            meta.insert(PASS_KEY.into(), pass.clone());
            // Preserve that extra `_meta` keys accompanied `pass` so the broker closes with `invalid_pass`.
            if output.meta_extra {
                meta.insert(EXTRA_KEY.into(), Value::Bool(true));
            }
        }
        map.insert("_meta".into(), Value::Object(meta));
    }
    canonicalize(&Value::Object(map))
}

/// Fixed error payload returned to the model on JSON-RPC errors without forwarding server text.
pub fn error_payload() -> String {
    "{\"content\":[{\"text\":\"tool_error\",\"type\":\"text\"}],\"isError\":true}".to_owned()
}

#[cfg(test)]
mod tests;
