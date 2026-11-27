//! MCP Streamable HTTP (2026-07-28) header encoding for `Mcp-Name` and `x-mcp-header` `Mcp-Param-*` arguments.
//!
//! - Values unsafe for HTTP headers (non-visible ASCII, leading/trailing whitespace, or matching the `=?base64?...?=` sentinel) are UTF-8 Base64 encoded.
//! - `x-mcp-header` is valid only on `string`, `integer`, or `boolean` properties reachable purely via `properties` from the schema root; absent or `null` arguments omit the header.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;

const SENTINEL: (&str, &str) = ("=?base64?", "?=");
/// JavaScript `Number.MAX_SAFE_INTEGER`.
const SAFE_INT: i64 = (1 << 53) - 1;

fn base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Encodes a header value as plain text when HTTP-safe, or `=?base64?<UTF-8 Base64>?=` otherwise.
pub fn encode_value(value: &str) -> String {
    let plain_char = |b: u8| (0x21..=0x7e).contains(&b) || b == b' ' || b == b'\t';
    let edge = |b: Option<&u8>| matches!(b, Some(b' ' | b'\t'));
    let bytes = value.as_bytes();
    let sentinel = value.starts_with(SENTINEL.0) && value.ends_with(SENTINEL.1);
    if bytes.iter().all(|b| plain_char(*b))
        && !edge(bytes.first())
        && !edge(bytes.last())
        && !sentinel
    {
        value.to_owned()
    } else {
        format!("{}{}{}", SENTINEL.0, base64(bytes), SENTINEL.1)
    }
}

/// RFC 9110 `token` (`tchar`).
fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

struct Mirror {
    path: Vec<String>,
    name: String,
    kind: &'static str,
}

fn mirrors(schema: &Value) -> Result<Vec<Mirror>, &'static str> {
    let mut found = Vec::new();
    scan(schema, Some(Vec::new()), &mut found)?;
    let mut seen: Vec<String> = Vec::new();
    for mirror in &found {
        let lower = mirror.name.to_ascii_lowercase();
        if seen.contains(&lower) {
            return Err("duplicate x-mcp-header name");
        }
        seen.push(lower);
    }
    Ok(found)
}

/// `chain` tracks the path through `properties` from root; becomes `None` once any other keyword is traversed.
fn scan(
    node: &Value,
    chain: Option<Vec<String>>,
    out: &mut Vec<Mirror>,
) -> Result<(), &'static str> {
    match node {
        Value::Object(map) => {
            if let Some(annotation) = map.get("x-mcp-header") {
                // The root schema itself (empty path) is not a property.
                let (Some(path), Some(name)) = (
                    chain.clone().filter(|path| !path.is_empty()),
                    annotation.as_str(),
                ) else {
                    return Err(
                        "x-mcp-header is only allowed on properties reachable via a root properties chain",
                    );
                };
                if !is_token(name) {
                    return Err("x-mcp-header must be a valid HTTP token");
                }
                let kind = match map.get("type").and_then(Value::as_str) {
                    Some("string") => "string",
                    Some("integer") => "integer",
                    Some("boolean") => "boolean",
                    _ => {
                        return Err(
                            "x-mcp-header is only allowed on string, integer, or boolean properties",
                        );
                    }
                };
                out.push(Mirror {
                    path,
                    name: name.to_owned(),
                    kind,
                });
            }
            for (key, child) in map {
                if key == "x-mcp-header" {
                    continue;
                }
                if key == "properties" {
                    let Value::Object(props) = child else {
                        continue;
                    };
                    for (name, prop) in props {
                        let next = chain.as_ref().map(|c| {
                            let mut c = c.clone();
                            c.push(name.clone());
                            c
                        });
                        scan(prop, next, out)?;
                    }
                } else {
                    scan(child, None, out)?;
                }
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(|item| scan(item, None, out)),
        _ => Ok(()),
    }
}

pub fn check_schema(schema: &Value) -> Result<(), &'static str> {
    mirrors(schema).map(|_| ())
}

pub fn param_headers(schema: &Value, args: &Value) -> Result<Vec<(String, String)>, &'static str> {
    let mut out = Vec::new();
    for mirror in mirrors(schema)? {
        let mut at = Some(args);
        for key in &mirror.path {
            at = at.and_then(|v| v.get(key));
        }
        let text = match (at, mirror.kind) {
            (None | Some(Value::Null), _) => continue,
            (Some(Value::String(text)), "string") => text.clone(),
            (Some(Value::Bool(flag)), "boolean") => flag.to_string(),
            (Some(Value::Number(n)), "integer") => match n.as_i64() {
                Some(i) if (-SAFE_INT..=SAFE_INT).contains(&i) => i.to_string(),
                _ => return Err("x-mcp-header integer is outside the safe range"),
            },
            _ => return Err("argument value does not match x-mcp-header property type"),
        };
        out.push((format!("Mcp-Param-{}", mirror.name), encode_value(&text)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_values_stay_and_the_rest_is_base64_with_the_sentinel() {
        assert_eq!(encode_value("us-west1"), "us-west1");
        assert_eq!(encode_value("a b\tc"), "a b\tc");
        assert_eq!(encode_value(""), "");
        // Spec example
        assert_eq!(
            encode_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
        assert_eq!(encode_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(encode_value("line1\nline2"), "=?base64?bGluZTEKbGluZTI=?=");
        assert_eq!(
            encode_value("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
        assert_eq!(encode_value("tab\t"), "=?base64?dGFiCQ==?=");
        for (input, want) in [
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("\u{e9}", "w6k="),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input}");
        }
    }

    fn schema() -> Value {
        json!({"type": "object", "properties": {
            "region": {"type": "string", "x-mcp-header": "Region"},
            "query": {"type": "string"},
            "n": {"type": "integer", "x-mcp-header": "Count"},
            "dry": {"type": "boolean", "x-mcp-header": "Dry-Run"},
            "nested": {"type": "object", "properties": {"zone": {"type": "string", "x-mcp-header": "Zone"}}}
        }})
    }

    #[test]
    fn headers_mirror_the_annotated_arguments_and_skip_absent_ones() {
        let args = json!({"region": "us-west1", "query": "SELECT 1", "n": -7, "dry": true, "nested": {"zone": "café"}});
        let got = param_headers(&schema(), &args).unwrap();
        let get = |name: &str| got.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        assert_eq!(get("Mcp-Param-Region"), Some("us-west1"));
        assert_eq!(get("Mcp-Param-Count"), Some("-7"));
        assert_eq!(get("Mcp-Param-Dry-Run"), Some("true"));
        assert!(get("Mcp-Param-Zone").unwrap().starts_with("=?base64?"));
        assert_eq!(got.len(), 4, "{got:?}");
        let got = param_headers(&schema(), &json!({"query": "q", "region": null})).unwrap();
        assert!(got.is_empty(), "{got:?}");
        assert!(
            param_headers(&json!({"type": "object"}), &json!({}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn misplaced_or_malformed_annotations_make_the_definition_invalid() {
        let bad = [
            json!({"type": "object", "properties": {"a": {"type": "array", "items": {"type": "string", "x-mcp-header": "A"}}}}),
            json!({"type": "object", "properties": {"a": {"anyOf": [{"type": "string", "x-mcp-header": "A"}]}}}),
            json!({"type": "object", "$defs": {"d": {"type": "string", "x-mcp-header": "A"}}}),
            json!({"type": "object", "properties": {"a": {"type": "number", "x-mcp-header": "A"}}}),
            json!({"type": "object", "properties": {"a": {"x-mcp-header": "A"}}}),
            json!({"type": "object", "properties": {"a": {"type": "string", "x-mcp-header": ""}}}),
            json!({"type": "object", "properties": {"a": {"type": "string", "x-mcp-header": "A B"}}}),
            json!({"type": "object", "properties": {"a": {"type": "string", "x-mcp-header": "A\r\nX: y"}}}),
            json!({"type": "object", "properties": {"a": {"type": "string", "x-mcp-header": 5}}}),
            json!({"type": "object", "properties": {
                "a": {"type": "string", "x-mcp-header": "Same"}, "b": {"type": "string", "x-mcp-header": "SAME"}}}),
            json!({"type": "string", "x-mcp-header": "Root"}),
        ];
        for schema in bad {
            assert!(check_schema(&schema).is_err(), "{schema}");
            assert!(param_headers(&schema, &json!({})).is_err(), "{schema}");
        }
        assert!(check_schema(&schema()).is_ok());
    }

    #[test]
    fn values_that_do_not_fit_the_annotated_type_are_refused() {
        let s = schema();
        for args in [
            json!({"region": 5}),
            json!({"n": "7"}),
            json!({"n": 1.5}),
            json!({"n": 9007199254740992i64}),
            json!({"dry": "true"}),
        ] {
            assert!(param_headers(&s, &args).is_err(), "{args}");
        }
        assert!(param_headers(&s, &json!({"n": 9007199254740991i64})).is_ok());
    }
}
