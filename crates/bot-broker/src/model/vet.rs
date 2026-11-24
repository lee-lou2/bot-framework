//! 상류로 보내기 전 요청 검사. 요청이 우리 쪽(MCP 중계로 등록된 클라이언트 도구)에서만 실행되고, 제공자가 원격 자원을 읽어 오지
//! 못하게 한다. 둘 다 로컬 승인·감사·egress 규칙 밖의 통로가 되기 때문이다. 허용목록 방식이라 모르는 것은 거절한다.

use bot_core::port::Error;
use serde_json::{Map, Value};

/// 최상위 요청 필드 허용목록. 제공자 쪽에서 실행되는 기능(`mcp_servers`·`container`·서버 도구 등)은 여기에 없다.
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

/// 경계 밖에서 실행을 여는 최상위 필드. 운영자가 `Provider.body_fields`에 적어도 열리지 않는다(확장 목록보다 먼저 거절한다).
/// `mcp_servers`는 제공자가 원격 MCP 서버를 직접 부르고, `container`는 제공자의 코드 실행 환경을 이어 쓴다.
const RESERVED: [&str; 2] = ["mcp_servers", "container"];

/// 도구 정의에서 받는 필드. `type`은 `custom`만, `input_schema`가 있어야 한다(제공자 도구에는 없다).
const TOOL_FIELDS: [&str; 6] = [
    "cache_control",
    "description",
    "input_schema",
    "name",
    "strict",
    "type",
];

/// 중첩한 `content`(도구 결과 안의 문서 안의 …)를 내려가 볼 깊이.
const MAX_DEPTH: usize = 8;

fn tool_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 요청이 우리 쪽에서만 실행되게 하고 원격 자원을 읽어 오지 못하게 한다:
/// 알 수 없는 최상위 필드, `type`이 `custom`이 아닌 도구(웹 검색·코드 실행 같은 서버 도구), `input_schema` 없는 도구,
/// 알 수 없는 `tool_choice`, 요청에 실려 오지 않은 첨부(원격 URL·제공자 파일 ID·컨테이너 파일)는 거절한다.
pub(super) fn vet_request(map: &Map<String, Value>, extra: &[String]) -> Result<(), Error> {
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

/// 메시지·시스템의 `content`: 글 하나이거나 블록 목록이다(도구 결과 안의 `content`도 같은 모양이라 같은 규칙으로 내려간다).
/// 객체 하나인 `content`(서버 도구 결과 등)도 블록으로 본다.
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

/// 이미지·문서 블록에서 받는 필드.
const IMAGE_FIELDS: [&str; 3] = ["cache_control", "source", "type"];
const DOCUMENT_FIELDS: [&str; 6] = [
    "cache_control",
    "citations",
    "context",
    "source",
    "title",
    "type",
];

/// 블록 하나. `tool_use`의 `input`은 모델이 만든 데이터라 들여다보지 않는다(거기에 `source` 같은 이름이 있어도 첨부가 아니다).
fn vet_block(block: &Value, depth: usize) -> Result<(), Error> {
    let block = block.as_object().ok_or(Error::Invalid)?;
    match block.get("type").and_then(Value::as_str) {
        Some("container_upload") => return Err(Error::Invalid), // 코드 실행 컨테이너의 파일을 가리킨다
        Some("image") => vet_media(block, &IMAGE_FIELDS, true, depth)?,
        Some("document") => vet_media(block, &DOCUMENT_FIELDS, false, depth)?,
        // 다른 블록의 글자 `source`(검색 결과의 출처 이름 등)는 가져오지 않는 이름표다. 객체 `source`는 같은 규칙으로 본다.
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

/// 이미지·문서: 알려 둔 필드만 있고, 객체 `source`가 있어야 한다. 이미지는 `base64`만 받는다.
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

/// 첨부(`source`)에서 받는 종류는 요청에 실려 온 `base64`·`text`·`content`뿐이다. `url`(제공자가 URL을 가져온다)과 `file`(제공자에
/// 올려 둔 파일 ID, 다른 프로필의 파일일 수 있다)은 요청 밖의 자원을 읽게 하므로 받지 않는다.
///
/// `source` 하나: 종류마다 필수 필드와 자료형이 갖춰져 있고 모르는 필드가 없을 때만 받는다(종류 이름만 맞는 빈 껍데기나
/// `url`·`file_id`를 슬쩍 끼운 것을 걸러낸다).
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

/// `type/subtype` 모양의 짧은 소문자 이름.
fn media_type_ok(value: Option<&Value>) -> bool {
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

/// 표준 base64: 글자표 안의 글자와 끝의 `=`(둘 이하)만, 비어 있지 않다. `:`·`/` 연속 같은 URL 모양은 `:`가 글자표에 없어 걸린다.
fn base64_ok(data: &str) -> bool {
    let body = data.trim_end_matches('=');
    !body.is_empty()
        && data.len() - body.len() <= 2
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

#[cfg(test)]
mod tests;
