//! Strict Gemini GenerateContent SSE and JSON observer extracting `call_mcp_tool` relay calls.

use super::{MAX_INPUT, Observed, ObservedUse, id_ok};
use crate::{canon::canonicalize, json::parse};
use bot_core::port::Error;
use serde_json::Value;

const RELAY_SERVER: &str = "relay";

#[derive(Default)]
pub(in crate::front) struct GeminiObserver {
    uses: Vec<ObservedUse>,
    stopped: bool,
    broken: bool,
}

fn has_function_call(parts: &[Value]) -> bool {
    parts.iter().any(|part| part.get("functionCall").is_some())
}

impl GeminiObserver {
    pub(super) fn feed(&mut self, event: &[u8]) {
        let Ok(text) = std::str::from_utf8(event) else {
            self.broken = true;
            return;
        };
        let mut data = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("data:")
                && data
                    .replace(rest.strip_prefix(' ').unwrap_or(rest))
                    .is_some()
            {
                self.broken = true;
                return;
            }
        }
        let Some(data) = data else {
            return;
        };
        let Some(value) = parse(data) else {
            self.broken = true;
            return;
        };
        self.inspect_chunk(&value);
    }

    fn inspect_chunk(&mut self, value: &Value) {
        let Some(candidates) = value.get("candidates").and_then(Value::as_array) else {
            return;
        };
        if candidates.iter().skip(1).any(|extra| {
            extra
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .is_some_and(|parts| has_function_call(parts))
        }) {
            self.broken = true;
            return;
        }
        let Some(candidate) = candidates.first() else {
            return;
        };
        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        {
            if self.stopped && has_function_call(parts) {
                self.broken = true;
                return;
            }
            if collect_parts(parts, &mut self.uses).is_err() {
                self.broken = true;
                return;
            }
        }
        if candidate
            .get("finishReason")
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.is_empty())
        {
            self.stopped = true;
        }
    }

    pub(super) fn stopped(&self) -> bool {
        self.stopped
    }

    pub(super) fn broken(&self) -> bool {
        self.broken
    }

    pub(super) fn finish(&mut self) -> Result<Option<Observed>, Error> {
        if self.broken || !self.stopped {
            return Err(Error::Invalid);
        }
        let uses = std::mem::take(&mut self.uses);
        if uses.is_empty() {
            return Ok(None);
        }
        // Final turn-scoped IDs (`agy-<turn>`, `agy-<turn>-<pos>`) are assigned on direct commit.
        Ok(Some(Observed {
            id: "agy-0".to_owned(),
            uses,
        }))
    }
}

fn collect_parts(parts: &[Value], out: &mut Vec<ObservedUse>) -> Result<(), Error> {
    for part in parts {
        let Some(call) = part.get("functionCall") else {
            continue;
        };
        let call = call.as_object().ok_or(Error::Invalid)?;
        let fn_name = call
            .get("name")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid)?;
        if fn_name != "call_mcp_tool" {
            // Built-in or non-MCP tool calls never enter `/mcp`.
            continue;
        }
        let args = call
            .get("args")
            .and_then(Value::as_object)
            .ok_or(Error::Invalid)?;
        let server = args
            .get("ServerName")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid)?;
        if server != RELAY_SERVER {
            return Err(Error::Invalid);
        }
        let tool_name = args
            .get("ToolName")
            .and_then(Value::as_str)
            .filter(|name| id_ok(name))
            .ok_or(Error::Invalid)?;
        let input = match args.get("Arguments") {
            None => "{}".to_owned(),
            Some(val @ Value::Object(_)) => {
                let canon = canonicalize(val).ok_or(Error::Invalid)?;
                if canon.len() > MAX_INPUT {
                    return Err(Error::Invalid);
                }
                canon
            }
            Some(_) => return Err(Error::Invalid),
        };
        let pos = out.len();
        out.push(ObservedUse {
            id: format!("agy-0-{pos}"),
            name: format!("mcp__{RELAY_SERVER}__{tool_name}"),
            input,
        });
    }
    Ok(())
}

pub(super) fn observe_body(body: &str) -> Result<Option<Observed>, Error> {
    let Some(value) = parse(body) else {
        return Err(Error::Invalid);
    };
    let mut uses = Vec::new();
    if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
        if candidates.iter().skip(1).any(|extra| {
            extra
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .is_some_and(|parts| has_function_call(parts))
        }) {
            return Err(Error::Invalid);
        }
        if let Some(parts) = candidates
            .first()
            .and_then(|c| c.pointer("/content/parts"))
            .and_then(Value::as_array)
        {
            collect_parts(parts, &mut uses)?;
        }
    }
    if uses.is_empty() {
        return Ok(None);
    }
    Ok(Some(Observed {
        id: "agy-0".to_owned(),
        uses,
    }))
}
