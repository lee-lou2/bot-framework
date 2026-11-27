//! Strict OpenAI `POST /v1/chat/completions` request parser and parameter validator.
use super::{
    error::Failure,
    strict::{self, Reject},
};
use bot_core::api::{ChatBody, ChatTurn, TurnRole};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chat {
    pub model: String,
    pub body: ChatBody,
    pub stream: bool,
    pub include_usage: bool,
}

/// Active fields (`model`, `messages`, `stream`, `stream_options`) plus validated-but-ignored schema fields.
const KNOWN: [&str; 12] = [
    "model",
    "messages",
    "stream",
    "stream_options",
    "n",
    "temperature",
    "top_p",
    "seed",
    "user",
    "metadata",
    "store",
    "response_format",
];

/// Nullable unsupported schema fields: ignored when `null`, rejected when set.
const NULLABLE_UNSUPPORTED: [&str; 13] = [
    "audio",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "max_completion_tokens",
    "max_tokens",
    "modalities",
    "prediction",
    "presence_penalty",
    "reasoning_effort",
    "service_tier",
    "stop",
    "top_logprobs",
];

pub fn parse(bytes: &[u8]) -> Result<Chat, Failure> {
    let value = strict::parse(bytes).map_err(|reject| match reject {
        Reject::Syntax => Failure::invalid("Request body is not valid JSON"),
        Reject::DuplicateKey => Failure::invalid("JSON object contains duplicate keys"),
    })?;
    let Value::Object(object) = value else {
        return Err(Failure::invalid("Request body must be a JSON object"));
    };
    for (key, value) in &object {
        let known = KNOWN.contains(&key.as_str());
        let ignorable = NULLABLE_UNSUPPORTED.contains(&key.as_str()) && value.is_null();
        if !known && !ignorable {
            return Err(unsupported_key("", key, &TOP_FIELDS));
        }
    }
    let model = model_of(&object)?;
    let body = ChatBody {
        turns: messages_of(&object)?,
    };
    let (stream, include_usage) = stream_of(&object)?;
    unused_fields(&object)?;
    Ok(Chat {
        model,
        body,
        stream,
        include_usage,
    })
}

fn invalid(param: &str, message: &'static str) -> Failure {
    Failure::invalid(message).with_param(param)
}

/// Allowlisted OpenAPI/SDK field names safe to echo in error `param`; unknown caller keys are never echoed.
const TOP_FIELDS: [&str; 35] = [
    "audio",
    "frequency_penalty",
    "function_call",
    "functions",
    "logit_bias",
    "logprobs",
    "max_completion_tokens",
    "max_tokens",
    "messages",
    "metadata",
    "modalities",
    "model",
    "n",
    "parallel_tool_calls",
    "prediction",
    "presence_penalty",
    "prompt_cache_key",
    "prompt_cache_retention",
    "reasoning_effort",
    "response_format",
    "safety_identifier",
    "seed",
    "service_tier",
    "stop",
    "store",
    "stream",
    "stream_options",
    "temperature",
    "tool_choice",
    "tools",
    "top_logprobs",
    "top_p",
    "user",
    "verbosity",
    "web_search_options",
];
const MESSAGE_FIELDS: [&str; 8] = [
    "audio",
    "content",
    "function_call",
    "name",
    "refusal",
    "role",
    "tool_call_id",
    "tool_calls",
];
const PART_FIELDS: [&str; 6] = [
    "file",
    "image_url",
    "input_audio",
    "refusal",
    "text",
    "type",
];
const STREAM_OPTION_FIELDS: [&str; 1] = ["include_usage"];
const RESPONSE_FORMAT_FIELDS: [&str; 2] = ["json_schema", "type"];

fn unsupported_key(base: &str, key: &str, known: &[&str]) -> Failure {
    match (base.is_empty(), known.contains(&key)) {
        (true, true) => Failure::unsupported(key),
        (false, true) => Failure::unsupported(format!("{base}.{key}")),
        (false, false) => Failure::unsupported(base),
        (true, false) => Failure::unsupported(""),
    }
}

fn model_of(object: &Map<String, Value>) -> Result<String, Failure> {
    match object.get("model") {
        Some(Value::String(model)) if !model.is_empty() && !model.contains('\0') => {
            Ok(model.clone())
        }
        _ => Err(invalid("model", "model must be a non-empty string")),
    }
}

fn text_of(param: &str, value: &Value) -> Result<String, Failure> {
    match value {
        Value::String(text) if !text.contains('\0') => Ok(text.clone()),
        _ => Err(invalid(param, "must be a string without NUL characters")),
    }
}

fn messages_of(object: &Map<String, Value>) -> Result<Vec<ChatTurn>, Failure> {
    let Some(Value::Array(list)) = object.get("messages").filter(|v| !v.is_null()) else {
        return Err(invalid("messages", "messages must be a non-empty array"));
    };
    if list.is_empty() {
        return Err(invalid("messages", "messages must be a non-empty array"));
    }
    let messages = list
        .iter()
        .enumerate()
        .map(|(at, item)| message_of(at, item))
        .collect::<Result<Vec<_>, _>>()?;
    let Some(last_user) = messages.iter().rposition(|m| m.role == TurnRole::User) else {
        return Err(invalid("messages", "at least one user message is required"));
    };
    if messages[last_user].content.trim().is_empty() {
        return Err(invalid(
            &format!("messages[{last_user}].content"),
            "last user message must not be empty",
        ));
    }
    Ok(messages)
}

fn message_of(at: usize, item: &Value) -> Result<ChatTurn, Failure> {
    let base = format!("messages[{at}]");
    let Value::Object(map) = item else {
        return Err(invalid(&base, "message must be an object"));
    };
    for key in map.keys() {
        if !matches!(key.as_str(), "role" | "content" | "name") {
            return Err(unsupported_key(&base, key, &MESSAGE_FIELDS));
        }
    }
    let role = match map.get("role") {
        Some(Value::String(role)) => match role.as_str() {
            "system" => TurnRole::System,
            "developer" => TurnRole::Developer,
            "user" => TurnRole::User,
            "assistant" => TurnRole::Assistant,
            "tool" | "function" => return Err(Failure::unsupported(format!("{base}.role"))),
            _ => return Err(invalid(&format!("{base}.role"), "unknown role")),
        },
        _ => return Err(invalid(&format!("{base}.role"), "role must be a string")),
    };
    let content_param = format!("{base}.content");
    let content = match map.get("content") {
        Some(Value::Array(parts)) if !parts.is_empty() => {
            let mut texts = Vec::with_capacity(parts.len());
            for (part_at, part) in parts.iter().enumerate() {
                texts.push(part_of(&format!("{content_param}[{part_at}]"), part)?);
            }
            texts.join("\n")
        }
        Some(Value::Array(_)) => {
            return Err(invalid(
                &content_param,
                "content parts array must not be empty",
            ));
        }
        Some(other) => text_of(&content_param, other)?,
        None => return Err(invalid(&content_param, "content is required")),
    };
    let name = match map.get("name") {
        None => None,
        Some(value) => Some(text_of(&format!("{base}.name"), value)?),
    };
    Ok(ChatTurn {
        role,
        content,
        name,
    })
}

fn part_of(base: &str, part: &Value) -> Result<String, Failure> {
    let Value::Object(map) = part else {
        return Err(invalid(base, "content part must be an object"));
    };
    // Check part type before unknown fields so non-text parts report unsupported type directly.
    match map.get("type") {
        Some(Value::String(kind)) if kind == "text" => {}
        Some(Value::String(_)) => return Err(Failure::unsupported(format!("{base}.type"))),
        _ => return Err(invalid(&format!("{base}.type"), "type must be a string")),
    }
    for key in map.keys() {
        if !matches!(key.as_str(), "type" | "text") {
            return Err(unsupported_key(base, key, &PART_FIELDS));
        }
    }
    match map.get("text") {
        Some(text) => text_of(&format!("{base}.text"), text),
        None => Err(invalid(&format!("{base}.text"), "text is required")),
    }
}

fn stream_of(object: &Map<String, Value>) -> Result<(bool, bool), Failure> {
    let stream = match object.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(stream)) => *stream,
        Some(_) => return Err(invalid("stream", "stream must be a boolean")),
    };
    let include_usage = match object.get("stream_options") {
        None | Some(Value::Null) => false,
        Some(Value::Object(options)) => {
            for key in options.keys() {
                if key != "include_usage" {
                    return Err(unsupported_key(
                        "stream_options",
                        key,
                        &STREAM_OPTION_FIELDS,
                    ));
                }
            }
            if !stream {
                return Err(invalid(
                    "stream_options",
                    "stream_options is only allowed when stream is true",
                ));
            }
            match options.get("include_usage") {
                None => false,
                Some(Value::Bool(flag)) => *flag,
                Some(_) => {
                    return Err(invalid(
                        "stream_options.include_usage",
                        "include_usage must be a boolean",
                    ));
                }
            }
        }
        Some(_) => {
            return Err(invalid(
                "stream_options",
                "stream_options must be an object",
            ));
        }
    };
    Ok((stream, include_usage))
}

/// Validates schema types/ranges for accepted-but-ignored parameters without using their values.
fn unused_fields(object: &Map<String, Value>) -> Result<(), Failure> {
    let number = |key: &str, low: f64, high: f64| -> Result<(), Failure> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(()),
            Some(Value::Number(n)) if n.as_f64().is_some_and(|x| (low..=high).contains(&x)) => {
                Ok(())
            }
            Some(_) => Err(invalid(key, "must be a number within the allowed range")),
        }
    };
    number("temperature", 0.0, 2.0)?;
    number("top_p", 0.0, 1.0)?;
    match object.get("seed") {
        None | Some(Value::Null) => {}
        Some(Value::Number(n))
            if n.is_i64() || n.as_u64().is_some_and(|u| i64::try_from(u).is_ok()) => {}
        Some(_) => return Err(invalid("seed", "seed must be an integer")),
    }
    // Official schema does not allow `null` for `user`.
    if object.get("user").is_some_and(|user| !user.is_string()) {
        return Err(invalid("user", "user must be a string"));
    }
    match object.get("metadata") {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) if map.values().all(Value::is_string) => {}
        Some(_) => {
            return Err(invalid(
                "metadata",
                "metadata must be an object of string values",
            ));
        }
    }
    match object.get("store") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => return Err(Failure::unsupported("store")),
        Some(_) => return Err(invalid("store", "store must be a boolean")),
    }
    match object.get("n") {
        None | Some(Value::Null) => {}
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => {
            if n.as_i64() != Some(1) {
                return Err(Failure::unsupported("n"));
            }
        }
        Some(_) => return Err(invalid("n", "n must be an integer")),
    }
    match object.get("response_format") {
        None => {}
        Some(Value::Object(map)) => {
            for key in map.keys() {
                if key != "type" {
                    return Err(unsupported_key(
                        "response_format",
                        key,
                        &RESPONSE_FORMAT_FIELDS,
                    ));
                }
            }
            match map.get("type") {
                Some(Value::String(kind)) if kind == "text" => {}
                Some(Value::String(_)) => {
                    return Err(Failure::unsupported("response_format.type"));
                }
                _ => {
                    return Err(invalid("response_format.type", "type must be a string"));
                }
            }
        }
        Some(_) => {
            return Err(invalid(
                "response_format",
                "response_format must be an object",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
