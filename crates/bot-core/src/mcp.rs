//! Canonical SHA-256 hashing and name validation for MCP tool definitions.
use crate::canon::{canonicalize, sha256};
use serde_json::{Map, Value};
use std::fmt::Write;

/// MCP tool name grammar: 1..=128 ASCII alphanumeric, `_`, `-`, or `.` (wider than profile kebab-case IDs).
pub fn is_tool_name(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Treats missing and `null` descriptions identically as `""`.
fn description_of(entry: &Map<String, Value>) -> Option<&str> {
    match entry.get("description") {
        None | Some(Value::Null) => Some(""),
        Some(Value::String(text)) => Some(text),
        Some(_) => None,
    }
}

/// Lowercase hex `sha256(JCS({"description", "inputSchema", "name"}))`; returns `None` if any number exceeds the safe JCS range. Includes `description` to detect tool-poisoning drift even when schemas match.
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

/// Hashes one `tools/list` entry; rejects entries whose name, description, or object `inputSchema` have unexpected types.
pub fn entry_sha256(entry: &Value) -> Option<String> {
    let entry = entry.as_object()?;
    let name = entry.get("name")?.as_str()?;
    let schema = entry
        .get("inputSchema")
        .filter(|schema| schema.is_object())?;
    definition_sha256(name, description_of(entry)?, schema)
}

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
            "café",
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
        let base = definition_sha256("get", "desc", &schema).unwrap();
        assert_eq!(base.len(), 64);
        let reordered = json!({"properties": {"a": {"type": "string"}}, "type": "object"});
        assert_eq!(definition_sha256("get", "desc", &reordered).unwrap(), base);
        assert_ne!(definition_sha256("put", "desc", &schema).unwrap(), base);
        assert_ne!(definition_sha256("get", "desc ", &schema).unwrap(), base);
        let other = json!({"type": "object", "properties": {"a": {"type": "integer"}}});
        assert_ne!(definition_sha256("get", "desc", &other).unwrap(), base);
    }

    #[test]
    fn an_entry_hashes_like_its_parts_and_a_missing_description_is_empty() {
        let schema = json!({"type": "object"});
        let entry =
            json!({"name": "get", "description": "d", "title": "ignored", "inputSchema": schema});
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
        let huge = json!({"type": "object", "maximum": 1e300});
        assert_eq!(definition_sha256("x", "", &huge), None);
    }
}
