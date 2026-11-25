//! MCP Streamable HTTP(2026-07-28)의 요청 머리 값. `Mcp-Name`과, 도구 입력 스키마의 `x-mcp-header` 주석이 가리키는 인자를 되풀이하는 `Mcp-Param-<이름>`.
//!
//! - 값은 HTTP 머리에 안전한 글(보이는 ASCII·공백·탭, 앞뒤 공백 없음)이 아니면 `=?base64?<UTF-8 Base64>?=`로 쓴다. 이 모양과 같은 평문 값도 Base64로 쓴다.
//! - `x-mcp-header`는 루트에서 `properties` 사슬로만 닿는 곳의 `string`·`integer`·`boolean` 속성에만 둘 수 있다(그 밖의 자리·이름 중복·토큰이 아닌 이름은 정의가
//!   잘못된 것이라 이 도구는 호출하지 않는다). 인자에 값이 없거나 `null`이면 머리를 만들지 않는다.
use serde_json::Value;

const SENTINEL: (&str, &str) = ("=?base64?", "?=");
/// JavaScript 안전 정수 범위
const SAFE_INT: i64 = (1 << 53) - 1;

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// 머리 값으로 쓸 글. 평문으로 안전하면 그대로, 아니면 Base64 표기.
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

/// RFC 9110 `token`(`tchar`)으로만 이뤄진 비어 있지 않은 글.
fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// 주석 한 건: (속성 경로, 머리 이름, 속성 타입)
struct Mirror {
    path: Vec<String>,
    name: String,
    kind: &'static str,
}

/// 스키마를 훑어 `x-mcp-header` 주석을 모은다. 주석이 허용되지 않는 자리에 있거나 모양이 틀리면 `Err`다.
fn mirrors(schema: &Value) -> Result<Vec<Mirror>, &'static str> {
    let mut found = Vec::new();
    scan(schema, Some(Vec::new()), &mut found)?;
    let mut seen: Vec<String> = Vec::new();
    for mirror in &found {
        let lower = mirror.name.to_ascii_lowercase();
        if seen.contains(&lower) {
            return Err("x-mcp-header 이름이 겹쳐요");
        }
        seen.push(lower);
    }
    Ok(found)
}

/// `chain`은 루트에서 `properties`로만 내려온 경로다(다른 키워드를 지나면 `None`).
fn scan(
    node: &Value,
    chain: Option<Vec<String>>,
    out: &mut Vec<Mirror>,
) -> Result<(), &'static str> {
    match node {
        Value::Object(map) => {
            if let Some(annotation) = map.get("x-mcp-header") {
                // 루트 자신(빈 경로)은 속성이 아니다
                let (Some(path), Some(name)) = (
                    chain.clone().filter(|path| !path.is_empty()),
                    annotation.as_str(),
                ) else {
                    return Err(
                        "x-mcp-header를 둘 수 없는 자리예요(properties 사슬로만 닿는 속성에 둬요)",
                    );
                };
                if !is_token(name) {
                    return Err("x-mcp-header가 HTTP 머리 이름이 아니에요");
                }
                let kind = match map.get("type").and_then(Value::as_str) {
                    Some("string") => "string",
                    Some("integer") => "integer",
                    Some("boolean") => "boolean",
                    _ => return Err("x-mcp-header는 string·integer·boolean 속성에만 둘 수 있어요"),
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

/// 이 입력 스키마가 `x-mcp-header`를 올바르게 쓰는가(동기화가 잠금에 적기 전에, 실행이 호출하기 전에 본다).
pub fn check_schema(schema: &Value) -> Result<(), &'static str> {
    mirrors(schema).map(|_| ())
}

/// 호출 인자(`args`)에서 `Mcp-Param-<이름>` 머리들을 만든다. 스키마의 주석이 잘못됐거나 인자 값이 주석의 타입과 맞지 않으면 `Err`다.
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
                _ => return Err("x-mcp-header 정수가 안전 범위 밖이에요"),
            },
            _ => return Err("인자 값이 x-mcp-header 속성의 타입과 달라요"),
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
        // 명세의 예
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
        // 패딩 길이별
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
        let args = json!({"region": "us-west1", "query": "SELECT 1", "n": -7, "dry": true, "nested": {"zone": "한국"}});
        let got = param_headers(&schema(), &args).unwrap();
        let get = |name: &str| got.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        assert_eq!(get("Mcp-Param-Region"), Some("us-west1"));
        assert_eq!(get("Mcp-Param-Count"), Some("-7"));
        assert_eq!(get("Mcp-Param-Dry-Run"), Some("true"));
        assert!(get("Mcp-Param-Zone").unwrap().starts_with("=?base64?"));
        assert_eq!(got.len(), 4, "{got:?}");
        // 값이 없거나 null이면 머리도 없다
        let got = param_headers(&schema(), &json!({"query": "q", "region": null})).unwrap();
        assert!(got.is_empty(), "{got:?}");
        // 주석이 없는 스키마는 머리가 없다
        assert!(
            param_headers(&json!({"type": "object"}), &json!({}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn misplaced_or_malformed_annotations_make_the_definition_invalid() {
        let bad = [
            // 배열 항목·조합 키워드·$defs 아래, number 타입, 타입 없음, 이름 문제
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
