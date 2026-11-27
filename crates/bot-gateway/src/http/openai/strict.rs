//! Strict JSON parsing for OpenAI requests that rejects duplicate keys and deep nesting.
use bot_core::json::{StrictError, parse_strict_with_depth};
use serde_json::Value;

const MAX_DEPTH: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reject {
    Syntax,
    DuplicateKey,
}

pub fn parse(bytes: &[u8]) -> Result<Value, Reject> {
    let text = std::str::from_utf8(bytes).map_err(|_| Reject::Syntax)?;
    parse_strict_with_depth(text, MAX_DEPTH).map_err(|error| match error {
        StrictError::DuplicateKey => Reject::DuplicateKey,
        StrictError::Syntax => Reject::Syntax,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_ordinary_json_exactly_like_serde_json() {
        for text in [
            r#"{"a":1,"b":[true,null,"x",1.5,-2,18446744073709551615],"c":{"d":{}}}"#,
            r#"[]"#,
            r#""café ★ A""#,
            r#"0"#,
            r#"null"#,
        ] {
            assert_eq!(
                parse(text.as_bytes()).unwrap(),
                serde_json::from_str::<Value>(text).unwrap(),
                "{text}"
            );
        }
        assert_eq!(parse(br#"{"a":1}"#).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn a_repeated_key_is_refused_at_any_depth_even_with_equal_values() {
        for text in [
            r#"{"a":1,"a":2}"#,
            r#"{"a":1,"a":1}"#,
            r#"{"x":{"a":1,"b":2,"a":3}}"#,
            r#"{"x":[{"a":1},{"b":1,"b":2}]}"#,
            r#"[{"k":null,"k":null}]"#,
        ] {
            assert_eq!(parse(text.as_bytes()), Err(Reject::DuplicateKey), "{text}");
        }
        assert!(parse(br#"{"a":{"k":1},"b":{"k":2},"c":[{"k":3},{"k":4}]}"#).is_ok());
    }

    #[test]
    fn anything_that_is_not_one_json_value_is_a_syntax_error() {
        for bytes in [
            &b""[..],
            b"{",
            b"{\"a\":1,}",
            b"{\"a\":1} x",
            b"{\"a\":1}{\"b\":2}",
            b"{'a':1}",
            b"NaN",
            b"{\"a\":Infinity}",
            b"\xff\xfe",
            b"{\"a\":\"\xff\"}",
        ] {
            assert_eq!(parse(bytes), Err(Reject::Syntax), "{bytes:?}");
        }
        let deep = format!("{}1{}", "[".repeat(5000), "]".repeat(5000));
        assert_eq!(parse(deep.as_bytes()), Err(Reject::Syntax));
    }
}
