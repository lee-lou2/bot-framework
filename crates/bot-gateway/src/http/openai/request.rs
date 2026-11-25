//! `POST /v1/chat/completions` 본문 검증. 고정한 공식 OpenAPI의 요청 스키마를 기준으로 **지원하는 것만** 받는다 — 지원하지 않는 필드·값을 조용히 무시하지 않고
//! 400 `unsupported_parameter`로 거절하고, 받되 쓰지 않는 필드(temperature 등)는 공식 타입·범위만 검사한다. 오류에는 필드 경로(`param`)만 싣고 값은 싣지 않는다.
//!
//! - 필수: `model`(문자열), `messages`(비어 있지 않은 배열; 역할 system·developer·user·assistant, 내용은 문자열 또는 text 파트 배열, 선택 `name`; user 메시지 하나 이상).
//! - 받아서 쓴다: `stream`, `stream_options.include_usage`(stream일 때만).
//! - 받되 쓰지 않는다(타입·범위 검사): `temperature`·`top_p`·`seed`·`user`·`metadata`·`store`(false만)·`response_format`(text만)·`n`(1만).
//! - 공식 스키마가 nullable로 허용한 선택 필드는 null이면 생략과 같다. 그 밖의 필드(알 수 없는 키 포함)는 null이어도 거절한다.
use super::{
    error::Failure,
    strict::{self, Reject},
};
use bot_core::api::{ChatBody, ChatTurn, TurnRole};
use serde_json::{Map, Value};

/// 검증한 요청. 대화는 core의 `ChatBody`(접수 본문과 같은 모양)이고, 내용 파트 배열은 `\n`으로 이어 붙인 문자열이다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chat {
    pub model: String,
    pub body: ChatBody,
    pub stream: bool,
    /// `stream_options.include_usage`(stream일 때만 참일 수 있다)
    pub include_usage: bool,
}

/// 받아서 쓰거나(model·messages·stream·stream_options) 받되 쓰지 않는(검사만) 필드
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

/// 공식 스키마가 nullable로 허용하는 «지원하지 않는» 선택 필드: null이면 생략과 같고 값이 있으면 거절한다
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
        Reject::Syntax => Failure::invalid("요청 본문이 올바른 JSON이 아니에요"),
        Reject::DuplicateKey => Failure::invalid("JSON 객체에 같은 키가 두 번 나와요"),
    })?;
    let Value::Object(object) = value else {
        return Err(Failure::invalid("요청 본문은 JSON 객체여야 해요"));
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

/// 오류의 `param`에 그대로 보여 줘도 되는 필드 이름: 고정한 OpenAPI 요청 스키마(와 공식 SDK 2.29.0이 더한 이름)에 있는 것뿐이다. 호출자가 지어낸 이름은
/// 비밀을 담았을 수 있으므로 오류 본문에 되풀이하지 않고 알려진 부모 경로까지만(최상위면 `param` 없이) 알린다.
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

/// 지원하지 않거나 알 수 없는 키의 오류. `known`은 그 자리에서 이름을 보여 줘도 되는 필드 목록이다.
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
        _ => Err(invalid("model", "model은 비어 있지 않은 문자열이어야 해요")),
    }
}

fn text_of(param: &str, value: &Value) -> Result<String, Failure> {
    match value {
        Value::String(text) if !text.contains('\0') => Ok(text.clone()),
        _ => Err(invalid(param, "문자열이어야 하고 NUL 문자를 쓸 수 없어요")),
    }
}

fn messages_of(object: &Map<String, Value>) -> Result<Vec<ChatTurn>, Failure> {
    let Some(Value::Array(list)) = object.get("messages").filter(|v| !v.is_null()) else {
        return Err(invalid(
            "messages",
            "messages는 비어 있지 않은 배열이어야 해요",
        ));
    };
    if list.is_empty() {
        return Err(invalid(
            "messages",
            "messages는 비어 있지 않은 배열이어야 해요",
        ));
    }
    let messages = list
        .iter()
        .enumerate()
        .map(|(at, item)| message_of(at, item))
        .collect::<Result<Vec<_>, _>>()?;
    let Some(last_user) = messages.iter().rposition(|m| m.role == TurnRole::User) else {
        return Err(invalid("messages", "user 메시지가 하나 이상 있어야 해요"));
    };
    if messages[last_user].content.trim().is_empty() {
        return Err(invalid(
            &format!("messages[{last_user}].content"),
            "마지막 user 메시지가 비어 있어요",
        ));
    }
    Ok(messages)
}

fn message_of(at: usize, item: &Value) -> Result<ChatTurn, Failure> {
    let base = format!("messages[{at}]");
    let Value::Object(map) = item else {
        return Err(invalid(&base, "메시지는 객체여야 해요"));
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
            _ => return Err(invalid(&format!("{base}.role"), "알 수 없는 역할이에요")),
        },
        _ => return Err(invalid(&format!("{base}.role"), "role은 문자열이어야 해요")),
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
                "내용 파트 배열은 비어 있을 수 없어요",
            ));
        }
        Some(other) => text_of(&content_param, other)?,
        None => return Err(invalid(&content_param, "content가 필요해요")),
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

/// 내용 파트: `{"type":"text","text":"…"}`만. 이미지·오디오·파일·거절(refusal) 파트와 알 수 없는 필드는 지원하지 않는다.
fn part_of(base: &str, part: &Value) -> Result<String, Failure> {
    let Value::Object(map) = part else {
        return Err(invalid(base, "내용 파트는 객체여야 해요"));
    };
    // 파트 종류를 먼저 본다: 이미지·오디오·파일 파트는 «종류가 지원되지 않음»이 가장 알려 주는 오류다
    match map.get("type") {
        Some(Value::String(kind)) if kind == "text" => {}
        Some(Value::String(_)) => return Err(Failure::unsupported(format!("{base}.type"))),
        _ => return Err(invalid(&format!("{base}.type"), "type은 문자열이어야 해요")),
    }
    for key in map.keys() {
        if !matches!(key.as_str(), "type" | "text") {
            return Err(unsupported_key(base, key, &PART_FIELDS));
        }
    }
    match map.get("text") {
        Some(text) => text_of(&format!("{base}.text"), text),
        None => Err(invalid(&format!("{base}.text"), "text가 필요해요")),
    }
}

fn stream_of(object: &Map<String, Value>) -> Result<(bool, bool), Failure> {
    let stream = match object.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(stream)) => *stream,
        Some(_) => return Err(invalid("stream", "stream은 불리언이어야 해요")),
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
                    "stream_options는 stream이 true일 때만 쓸 수 있어요",
                ));
            }
            match options.get("include_usage") {
                None => false,
                Some(Value::Bool(flag)) => *flag,
                Some(_) => {
                    return Err(invalid(
                        "stream_options.include_usage",
                        "include_usage는 불리언이어야 해요",
                    ));
                }
            }
        }
        Some(_) => return Err(invalid("stream_options", "stream_options는 객체여야 해요")),
    };
    Ok((stream, include_usage))
}

/// 받되 쓰지 않는 필드: 공식 타입·범위만 본다(실행·신원·라우팅·로그에 쓰지 않는다)
fn unused_fields(object: &Map<String, Value>) -> Result<(), Failure> {
    let number = |key: &str, low: f64, high: f64| -> Result<(), Failure> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(()),
            Some(Value::Number(n)) if n.as_f64().is_some_and(|x| (low..=high).contains(&x)) => {
                Ok(())
            }
            Some(_) => Err(invalid(key, "허용 범위의 숫자여야 해요")),
        }
    };
    number("temperature", 0.0, 2.0)?;
    number("top_p", 0.0, 1.0)?;
    match object.get("seed") {
        None | Some(Value::Null) => {}
        Some(Value::Number(n))
            if n.is_i64() || n.as_u64().is_some_and(|u| i64::try_from(u).is_ok()) => {}
        Some(_) => return Err(invalid("seed", "seed는 정수여야 해요")),
    }
    // 공식 스키마에서 `user`는 null을 허용하지 않는다
    if object.get("user").is_some_and(|user| !user.is_string()) {
        return Err(invalid("user", "user는 문자열이어야 해요"));
    }
    match object.get("metadata") {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) if map.values().all(Value::is_string) => {}
        Some(_) => return Err(invalid("metadata", "metadata는 문자열 값의 객체여야 해요")),
    }
    match object.get("store") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => return Err(Failure::unsupported("store")),
        Some(_) => return Err(invalid("store", "store는 불리언이어야 해요")),
    }
    match object.get("n") {
        None | Some(Value::Null) => {}
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => {
            if n.as_i64() != Some(1) {
                return Err(Failure::unsupported("n"));
            }
        }
        Some(_) => return Err(invalid("n", "n은 정수여야 해요")),
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
                    return Err(invalid("response_format.type", "type은 문자열이어야 해요"));
                }
            }
        }
        Some(_) => {
            return Err(invalid(
                "response_format",
                "response_format은 객체여야 해요",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
