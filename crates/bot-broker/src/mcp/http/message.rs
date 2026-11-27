//! JSON-RPC message extraction, secret scanning, and status mapping for HTTP MCP responses.
use crate::{guard::contains_bytes, json::parse, mcp::McpError, model::ChunkSource};
use serde_json::Value;

const MAX_ERROR_BODY: usize = 64 << 10;

pub(super) enum Message {
    Result(Value),
    Error { code: i64, data: Option<Value> },
    Empty,
}

pub(super) struct Answer {
    pub(super) status: u16,
    pub(super) session: Option<String>,
    pub(super) message: Message,
}

pub(super) fn media_type(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Validates 1..=512 visible ASCII bytes safe to echo in HTTP headers.
pub(super) fn session_ok(text: &str) -> bool {
    (1..=512).contains(&text.len()) && text.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

pub(super) async fn read_all(body: &mut dyn ChunkSource, max: usize) -> Result<Vec<u8>, McpError> {
    let mut out = Vec::new();
    loop {
        match body.next().await {
            Ok(Some(chunk)) => {
                if out.len() + chunk.len() > max {
                    return Err(McpError::Protocol);
                }
                out.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(out),
            Err(_) => return Err(McpError::Io),
        }
    }
}

pub(super) fn holds_bytes(raw: &[u8], secret: Option<&str>) -> bool {
    secret.is_some_and(|secret| contains_bytes(raw, secret.as_bytes()))
}

/// Scans decoded strings, keys, and numbers to catch JSON-escaped secrets alongside `holds_bytes`.
pub(super) fn holds(value: &Value, secret: Option<&str>) -> bool {
    let Some(secret) = secret else {
        return false;
    };
    fn walk(value: &Value, secret: &[u8]) -> bool {
        match value {
            Value::String(text) => contains_bytes(text.as_bytes(), secret),
            Value::Number(number) => contains_bytes(number.to_string().as_bytes(), secret),
            Value::Array(items) => items.iter().any(|item| walk(item, secret)),
            Value::Object(map) => map
                .iter()
                .any(|(key, item)| contains_bytes(key.as_bytes(), secret) || walk(item, secret)),
            _ => false,
        }
    }
    walk(value, secret.as_bytes())
}

/// Extracts a matching JSON-RPC error (`id` equal or `null`) from a non-2xx body; fails with `Exposed` if `secret` appears.
pub(super) async fn error_message(
    body: &mut dyn ChunkSource,
    id: u64,
    secret: Option<&str>,
) -> Result<Message, McpError> {
    let Ok(bytes) = read_all(body, MAX_ERROR_BODY).await else {
        return Ok(Message::Empty);
    };
    if holds_bytes(&bytes, secret) {
        return Err(McpError::Exposed);
    }
    let Some(value) = std::str::from_utf8(&bytes).ok().and_then(parse) else {
        return Ok(Message::Empty);
    };
    if holds(&value, secret) {
        return Err(McpError::Exposed);
    }
    let ours = match value.get("id") {
        Some(Value::Null) => true,
        Some(other) => other.as_u64() == Some(id),
        None => false,
    };
    let code = value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64);
    Ok(match code {
        Some(code) if ours => Message::Error {
            code,
            data: value.get("error").and_then(|e| e.get("data")).cloned(),
        },
        _ => Message::Empty,
    })
}

/// Parses one JSON-RPC response; returns `Ok(None)` for unrelated request IDs.
pub(super) fn response_message(value: &Value, id: u64) -> Result<Option<Message>, McpError> {
    let obj = value.as_object().ok_or(McpError::Protocol)?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || obj.contains_key("method") {
        return Err(McpError::Protocol);
    }
    if obj.get("id").and_then(Value::as_u64) != Some(id) {
        return Ok(None);
    }
    if let Some(error) = obj.get("error") {
        return Ok(Some(Message::Error {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            data: error.get("data").cloned(),
        }));
    }
    match obj.get("result") {
        Some(result @ Value::Object(_)) => Ok(Some(Message::Result(result.clone()))),
        _ => Err(McpError::Protocol),
    }
}

pub(super) fn listed(value: Option<&Value>) -> Vec<&str> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Accepts only `complete` (or omitted, for older servers) `resultType` on 2xx responses.
pub(super) fn finish(answer: Answer) -> Result<Value, McpError> {
    let ok = (200..300).contains(&answer.status);
    match answer.message {
        Message::Result(result) if ok => match result.get("resultType") {
            None => Ok(result),
            Some(Value::String(kind)) if kind == "complete" => Ok(result),
            Some(_) => Err(McpError::Protocol),
        },
        Message::Error { code, .. } if ok => Err(McpError::Server(code)),
        Message::Error { code, .. } => Err(McpError::Status {
            status: answer.status,
            code: Some(code),
        }),
        _ if ok => Err(McpError::Protocol),
        _ => Err(McpError::Status {
            status: answer.status,
            code: None,
        }),
    }
}
