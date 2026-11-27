//! OpenAI Responses API request allowlist vetting for Codex CLI relay requests.

use super::{MAX_DEPTH, base64_ok, media_type_ok, tool_name_ok};
use bot_core::port::Error;
use serde_json::{Map, Value};

const BODY_FIELDS: [&str; 20] = [
    "client_metadata",
    "include",
    "input",
    "instructions",
    "max_output_tokens",
    "metadata",
    "model",
    "parallel_tool_calls",
    "prompt_cache_key",
    "reasoning",
    "service_tier",
    "store",
    "stream",
    "temperature",
    "text",
    "tool_choice",
    "tools",
    "top_p",
    "truncation",
    "user",
];

/// Fields that enable server-side state or provider-side execution; rejected even if operator-listed.
const RESERVED: [&str; 4] = [
    "background",
    "previous_response_id",
    "mcp_servers",
    "container",
];

const FUNCTION_FIELDS: [&str; 5] = ["type", "name", "description", "parameters", "strict"];
const NAMESPACE_FIELDS: [&str; 4] = ["type", "name", "description", "tools"];

pub(super) fn vet_request(map: &Map<String, Value>, extra: &[String]) -> Result<(), Error> {
    if map.keys().any(|k| {
        RESERVED.contains(&k.as_str())
            || (!BODY_FIELDS.contains(&k.as_str()) && !extra.iter().any(|e| e == k))
    }) {
        return Err(Error::Invalid);
    }
    if let Some(store) = map.get("store")
        && store != &Value::Bool(false)
    {
        return Err(Error::Invalid);
    }
    if let Some(tools) = map.get("tools") {
        vet_tools(tools)?;
    }
    if let Some(choice) = map.get("tool_choice") {
        vet_choice(choice)?;
    }
    if let Some(input) = map.get("input") {
        vet_input(input)?;
    }
    Ok(())
}

fn vet_tools(tools: &Value) -> Result<(), Error> {
    for tool in tools.as_array().ok_or(Error::Invalid)? {
        let tool = tool.as_object().ok_or(Error::Invalid)?;
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => vet_function_tool(tool)?,
            Some("namespace") => vet_namespace_tool(tool)?,
            _ => return Err(Error::Invalid),
        }
    }
    Ok(())
}

fn vet_function_tool(tool: &Map<String, Value>) -> Result<(), Error> {
    let named = tool
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(tool_name_ok);
    let params_ok = tool.get("parameters").is_none_or(Value::is_object);
    let strict_ok = tool.get("strict").is_none_or(Value::is_boolean);
    if !named
        || !params_ok
        || !strict_ok
        || tool.keys().any(|k| !FUNCTION_FIELDS.contains(&k.as_str()))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

fn vet_namespace_tool(tool: &Map<String, Value>) -> Result<(), Error> {
    let named = tool
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(tool_name_ok);
    if !named || tool.keys().any(|k| !NAMESPACE_FIELDS.contains(&k.as_str())) {
        return Err(Error::Invalid);
    }
    let nested = tool
        .get("tools")
        .and_then(Value::as_array)
        .ok_or(Error::Invalid)?;
    for entry in nested {
        let entry = entry.as_object().ok_or(Error::Invalid)?;
        if entry.get("type").and_then(Value::as_str) != Some("function") {
            return Err(Error::Invalid);
        }
        vet_function_tool(entry)?;
    }
    Ok(())
}

fn vet_choice(choice: &Value) -> Result<(), Error> {
    match choice {
        Value::String(mode) if matches!(mode.as_str(), "auto" | "none" | "required") => Ok(()),
        Value::Object(map) => {
            let kind = map.get("type").and_then(Value::as_str);
            let named = map
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(tool_name_ok);
            if kind == Some("function") && named && map.keys().all(|k| k == "type" || k == "name") {
                Ok(())
            } else {
                Err(Error::Invalid)
            }
        }
        _ => Err(Error::Invalid),
    }
}

fn vet_input(input: &Value) -> Result<(), Error> {
    match input {
        Value::String(_) => Ok(()),
        Value::Array(items) => items.iter().try_for_each(vet_input_item),
        _ => Err(Error::Invalid),
    }
}

fn vet_input_item(item: &Value) -> Result<(), Error> {
    let obj = item.as_object().ok_or(Error::Invalid)?;
    // Reject item-level references to provider-hosted files or remote URLs.
    if obj.contains_key("file_id")
        || obj.contains_key("file_url")
        || obj.contains_key("file_data")
        || obj.contains_key("image_url")
    {
        return Err(Error::Invalid);
    }
    match obj.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            if obj.contains_key("content") || obj.contains_key("output") {
                return Err(Error::Invalid);
            }
            Ok(())
        }
        Some("function_call_output") => {
            if obj.contains_key("content") {
                return Err(Error::Invalid);
            }
            if let Some(output) = obj.get("output") {
                vet_content(output, 0)?;
            }
            Ok(())
        }
        Some("message") | None => {
            if obj.contains_key("output") {
                return Err(Error::Invalid);
            }
            if let Some(content) = obj.get("content") {
                vet_content(content, 0)?;
            }
            Ok(())
        }
        Some("reasoning") => {
            if obj.contains_key("output") {
                return Err(Error::Invalid);
            }
            if let Some(content) = obj.get("content") {
                vet_content(content, 0)?;
            }
            if let Some(summary) = obj.get("summary") {
                vet_content(summary, 0)?;
            }
            Ok(())
        }
        // Reject `item_reference`, `computer_call_output`, direct `input_image`, and unknown item types.
        Some(_) => Err(Error::Invalid),
    }
}

fn vet_content(content: &Value, depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid);
    }
    match content {
        Value::String(_) => Ok(()),
        Value::Array(parts) => vet_parts(parts, depth),
        Value::Object(_) => vet_part(content, depth),
        _ => Err(Error::Invalid),
    }
}

fn vet_parts(parts: &[Value], depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid);
    }
    parts.iter().try_for_each(|part| vet_part(part, depth))
}

fn vet_part(part: &Value, depth: usize) -> Result<(), Error> {
    let obj = part.as_object().ok_or(Error::Invalid)?;
    if obj.contains_key("file_id") || obj.contains_key("file_url") {
        return Err(Error::Invalid);
    }
    match obj.get("type").and_then(Value::as_str) {
        Some("input_image") => {
            let url = obj
                .get("image_url")
                .and_then(Value::as_str)
                .ok_or(Error::Invalid)?;
            if !data_uri_ok(url) || obj.contains_key("file_data") {
                return Err(Error::Invalid);
            }
        }
        Some("input_file") => {
            let data = obj
                .get("file_data")
                .and_then(Value::as_str)
                .ok_or(Error::Invalid)?;
            if (!data_uri_ok(data) && !base64_ok(data)) || obj.contains_key("image_url") {
                return Err(Error::Invalid);
            }
        }
        _ => {
            if obj.contains_key("file_data") {
                return Err(Error::Invalid);
            }
            if let Some(url) = obj.get("image_url") {
                let raw = match url {
                    Value::String(s) => s.as_str(),
                    Value::Object(m) => {
                        m.get("url").and_then(Value::as_str).ok_or(Error::Invalid)?
                    }
                    _ => return Err(Error::Invalid),
                };
                if !data_uri_ok(raw) {
                    return Err(Error::Invalid);
                }
            }
        }
    }
    if let Some(inner) = obj.get("content") {
        vet_content(inner, depth + 1)?;
    }
    Ok(())
}

/// Validates inline `data:<mime>;base64,<payload>` URIs and rejects remote URLs.
fn data_uri_ok(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("data:") else {
        return false;
    };
    let Some((mime, b64)) = rest.split_once(";base64,") else {
        return false;
    };
    media_type_ok(Some(&Value::String(mime.to_owned()))) && base64_ok(b64)
}
