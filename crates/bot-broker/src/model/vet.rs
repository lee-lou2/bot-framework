//! Allowlist validation for outbound model requests to block provider-side tools and remote fetches.
//!
//! Rejects unknown top-level fields, server-side tools, and external attachments so every
//! tool call and resource fetch remains under local approval, audit, and egress controls.

mod gemini;
mod responses;

use super::Protocol;
use bot_core::port::Error;
use serde_json::{Map, Value};

/// Permitted top-level fields; excludes provider-executed features (`mcp_servers`, `container`, server tools).
const BODY_FIELDS: [&str; 15] = [
    "context_management",
    "max_tokens",
    "messages",
    "metadata",
    "model",
    "output_config",
    "stop_sequences",
    "stream",
    "system",
    "temperature",
    "thinking",
    "tool_choice",
    "tools",
    "top_k",
    "top_p",
];

/// Fields enabling out-of-boundary execution; rejected even if listed in `Provider.body_fields`.
const RESERVED: [&str; 2] = ["mcp_servers", "container"];

/// Permitted tool fields; only `type: "custom"` with an `input_schema` is accepted.
const TOOL_FIELDS: [&str; 6] = [
    "cache_control",
    "description",
    "input_schema",
    "name",
    "strict",
    "type",
];

/// Maximum nesting depth when traversing `content` blocks.
pub(super) const MAX_DEPTH: usize = 8;

pub(super) fn tool_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(super) fn vet_request(
    protocol: Protocol,
    map: &Map<String, Value>,
    extra: &[String],
) -> Result<(), Error> {
    match protocol {
        Protocol::Anthropic => vet_anthropic(map, extra),
        Protocol::OpenAiResponses => responses::vet_request(map, extra),
        Protocol::Gemini => gemini::vet_request(map, extra),
    }
}

fn vet_anthropic(map: &Map<String, Value>, extra: &[String]) -> Result<(), Error> {
    if map.keys().any(|k| {
        RESERVED.contains(&k.as_str())
            || (!BODY_FIELDS.contains(&k.as_str()) && !extra.iter().any(|e| e == k))
    }) {
        return Err(Error::Invalid);
    }
    if let Some(tools) = map.get("tools") {
        vet_tools(tools)?;
    }
    if let Some(choice) = map.get("tool_choice") {
        vet_choice(choice)?;
    }
    if let Some(system) = map.get("system") {
        vet_content(system, 0)?;
    }
    if let Some(messages) = map.get("messages") {
        for message in messages.as_array().ok_or(Error::Invalid)? {
            let message = message.as_object().ok_or(Error::Invalid)?;
            if let Some(content) = message.get("content") {
                vet_content(content, 0)?;
            }
        }
    }
    Ok(())
}

fn vet_tools(tools: &Value) -> Result<(), Error> {
    for tool in tools.as_array().ok_or(Error::Invalid)? {
        let tool = tool.as_object().ok_or(Error::Invalid)?;
        let named = tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(tool_name_ok);
        let custom = tool.get("type").is_none_or(|t| t == "custom");
        let schema = tool.get("input_schema").is_some_and(Value::is_object);
        if !named || !custom || !schema || tool.keys().any(|k| !TOOL_FIELDS.contains(&k.as_str())) {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}

fn vet_choice(choice: &Value) -> Result<(), Error> {
    let choice = choice.as_object().ok_or(Error::Invalid)?;
    let kind = choice
        .get("type")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid)?;
    let named = choice
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(tool_name_ok);
    let extras_ok = choice
        .keys()
        .all(|k| matches!(k.as_str(), "type" | "name" | "disable_parallel_tool_use"));
    if !extras_ok || !matches!(kind, "auto" | "any" | "none" | "tool") || (kind == "tool") != named
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

/// Validates string, block-array, or single-object `content` recursively up to `MAX_DEPTH`.
fn vet_content(content: &Value, depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid);
    }
    match content {
        Value::String(_) => Ok(()),
        Value::Array(blocks) => blocks.iter().try_for_each(|block| vet_block(block, depth)),
        Value::Object(_) => vet_block(content, depth),
        _ => Err(Error::Invalid),
    }
}

const IMAGE_FIELDS: [&str; 3] = ["cache_control", "source", "type"];
const DOCUMENT_FIELDS: [&str; 6] = [
    "cache_control",
    "citations",
    "context",
    "source",
    "title",
    "type",
];

/// `tool_use` inputs are opaque model data and are intentionally not inspected as attachments.
fn vet_block(block: &Value, depth: usize) -> Result<(), Error> {
    let block = block.as_object().ok_or(Error::Invalid)?;
    match block.get("type").and_then(Value::as_str) {
        Some("container_upload") => return Err(Error::Invalid), // Refers to a file inside a provider code-execution container.
        Some("image") => vet_media(block, &IMAGE_FIELDS, true, depth)?,
        Some("document") => vet_media(block, &DOCUMENT_FIELDS, false, depth)?,
        // String `source` values (e.g. citation labels) are inert; object `source` blocks are vetted.
        _ => {
            if let Some(Value::Object(source)) = block.get("source") {
                vet_source(source, false, depth)?;
            }
        }
    }
    if let Some(inner) = block.get("content") {
        vet_content(inner, depth + 1)?;
    }
    Ok(())
}

fn vet_media(
    block: &Map<String, Value>,
    fields: &[&str],
    base64_only: bool,
    depth: usize,
) -> Result<(), Error> {
    if block.keys().any(|k| !fields.contains(&k.as_str())) {
        return Err(Error::Invalid);
    }
    match block.get("source") {
        Some(Value::Object(source)) => vet_source(source, base64_only, depth),
        _ => Err(Error::Invalid),
    }
}

/// Allows only inline `base64`, `text`, or `content` sources; rejects `url` and `file` to block external or cross-profile reads.
fn vet_source(source: &Map<String, Value>, base64_only: bool, depth: usize) -> Result<(), Error> {
    let kind = source
        .get("type")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid)?;
    let fields: &[&str] = match kind {
        "base64" | "text" => &["type", "media_type", "data"],
        "content" => &["type", "content"],
        _ => return Err(Error::Invalid),
    };
    if source.keys().any(|k| !fields.contains(&k.as_str())) {
        return Err(Error::Invalid);
    }
    let data = source.get("data").and_then(Value::as_str);
    match kind {
        "base64" if media_type_ok(source.get("media_type")) => {
            if data.is_some_and(base64_ok) {
                Ok(())
            } else {
                Err(Error::Invalid)
            }
        }
        "text" if !base64_only && media_type_ok(source.get("media_type")) && data.is_some() => {
            Ok(())
        }
        "content" if !base64_only => match source.get("content") {
            Some(inner) => vet_content(inner, depth + 1),
            None => Err(Error::Invalid),
        },
        _ => Err(Error::Invalid),
    }
}

pub(super) fn media_type_ok(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|m| {
        m.len() <= 100
            && m.split_once('/')
                .is_some_and(|(kind, sub)| !kind.is_empty() && !sub.is_empty())
            && m.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'/' | b'.' | b'+' | b'-')
            })
    })
}

/// Standard base64 without whitespace; rejects URL-like strings since `:` is outside the alphabet.
pub(super) fn base64_ok(data: &str) -> bool {
    let body = data.trim_end_matches('=');
    !body.is_empty()
        && data.len() - body.len() <= 2
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

#[cfg(test)]
mod multi_protocol_tests;
#[cfg(test)]
mod tests;
