//! MCP 도구 정의의 고정 해시. 서버가 `tools/list`로 내세우는 도구 하나(이름·설명·입력 스키마)를 한 값으로 묶어, 프로필의 `tools.lock.yaml`에 적어 둔 값과
//! 실행 때 서버가 실제로 내세우는 값이 같은지 본다. 설명도 해시에 든다(설명에 숨긴 지시로 모델을 속이는 «도구 오염»은 스키마가 같아도 설명만 바꾸면 되기 때문이다).
//!
//! 프로필 검증·동기화(`bot mcp sync`)·브로커의 실행 대조가 모두 이 함수 하나를 쓴다. 정규화는 RFC 8785(`canon`)다.
use crate::canon::{canonicalize, sha256};
use serde_json::{Map, Value};
use std::fmt::Write;

/// MCP 규격의 도구 이름 규칙: 1~128자, 영문 대소문자·숫자·`_`·`-`·`.`만. tools.yaml `name`과 `tools.lock.yaml`이 쓴다(프로필 도구 id는 더 좁은 kebab-case다).
pub fn is_tool_name(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// 해시 입력의 설명. 설명이 없거나 `null`이면 빈 글이다(없음과 빈 글을 같게 본다).
fn description_of(entry: &Map<String, Value>) -> Option<&str> {
    match entry.get("description") {
        None | Some(Value::Null) => Some(""),
        Some(Value::String(text)) => Some(text),
        Some(_) => None,
    }
}

/// `sha256(JCS({"description", "inputSchema", "name"}))`을 소문자 16진으로. 정규화할 수 없는 수(안전 범위 밖)가 있으면 `None`.
pub fn definition_sha256(name: &str, description: &str, input_schema: &Value) -> Option<String> {
    let mut map = Map::new();
    map.insert("name".into(), Value::String(name.to_owned()));
    map.insert("description".into(), Value::String(description.to_owned()));
    map.insert("inputSchema".into(), input_schema.clone());
    let text = canonicalize(&Value::Object(map))?;
    let mut out = String::with_capacity(64);
    for byte in sha256(&text) {
        let _ = write!(out, "{byte:02x}");
    }
    Some(out)
}

/// 서버가 `tools/list`로 준 도구 한 건에서 같은 해시를 계산한다. 이름이 글이 아니거나 입력 스키마가 객체가 아니거나 설명이 글이 아니면 `None`(모양이 틀린 정의는 믿지 않는다).
pub fn entry_sha256(entry: &Value) -> Option<String> {
    let entry = entry.as_object()?;
    let name = entry.get("name")?.as_str()?;
    let schema = entry
        .get("inputSchema")
        .filter(|schema| schema.is_object())?;
    definition_sha256(name, description_of(entry)?, schema)
}

/// 서버가 준 도구 한 건의 설명(없으면 빈 글). 모양이 틀리면 `None`.
pub fn entry_description(entry: &Value) -> Option<String> {
    description_of(entry.as_object()?).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_names_follow_the_mcp_rule() {
        for ok in [
            "get_forecast",
            "get-forecast",
            "Get.Forecast_2",
            "a",
            &"x".repeat(128),
        ] {
            assert!(is_tool_name(ok), "{ok}");
        }
        for bad in [
            "",
            "has space",
            "a,b",
            "한글",
            "a/b",
            "a:b",
            &"x".repeat(129),
        ] {
            assert!(!is_tool_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_hash_binds_name_description_and_schema_and_ignores_key_order() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        let base = definition_sha256("get", "설명", &schema).unwrap();
        assert_eq!(base.len(), 64);
        let reordered = json!({"properties": {"a": {"type": "string"}}, "type": "object"});
        assert_eq!(definition_sha256("get", "설명", &reordered).unwrap(), base);
        assert_ne!(definition_sha256("put", "설명", &schema).unwrap(), base);
        assert_ne!(definition_sha256("get", "설명 ", &schema).unwrap(), base);
        let other = json!({"type": "object", "properties": {"a": {"type": "integer"}}});
        assert_ne!(definition_sha256("get", "설명", &other).unwrap(), base);
    }

    #[test]
    fn an_entry_hashes_like_its_parts_and_a_missing_description_is_empty() {
        let schema = json!({"type": "object"});
        let entry =
            json!({"name": "get", "description": "d", "title": "무시", "inputSchema": schema});
        assert_eq!(
            entry_sha256(&entry),
            definition_sha256("get", "d", &json!({"type": "object"}))
        );
        let bare = json!({"name": "get", "inputSchema": {"type": "object"}});
        assert_eq!(
            entry_sha256(&bare),
            definition_sha256("get", "", &json!({"type": "object"}))
        );
        let null = json!({"name": "get", "description": null, "inputSchema": {"type": "object"}});
        assert_eq!(entry_sha256(&null), entry_sha256(&bare));
    }

    #[test]
    fn malformed_entries_are_never_hashed() {
        for entry in [
            json!({"description": "d", "inputSchema": {}}),
            json!({"name": 1, "inputSchema": {}}),
            json!({"name": "x"}),
            json!({"name": "x", "inputSchema": null}),
            json!({"name": "x", "inputSchema": []}),
            json!({"name": "x", "description": 5, "inputSchema": {}}),
            json!("x"),
        ] {
            assert_eq!(entry_sha256(&entry), None, "{entry}");
        }
        // 안전 범위 밖의 수는 정규화할 수 없다
        let huge = json!({"type": "object", "maximum": 1e300});
        assert_eq!(definition_sha256("x", "", &huge), None);
    }
}
