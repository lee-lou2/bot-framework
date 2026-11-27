//! 엄격한 JSON 읽기: 같은 객체 안의 **중복 키**(깊이와 무관)를 거절한다. `serde_json::from_slice`는 중복 키를 조용히 받아 마지막 값을 쓰므로,
//! 검증이 본 값과 실행이 받는 값이 갈리는 길을 막으려고 `bot_core::json::parse_strict_with_depth`로 읽는다.
//! 문법 오류·뒤따르는 글·잘못된 UTF-8·너무 깊은 중첩도 오류다.

use bot_core::json::{StrictError, parse_strict_with_depth};
use serde_json::Value;

/// OpenAI 입구 JSON의 중첩 깊이 상한(`serde_json` 기본 재귀 한도와 같다).
const MAX_DEPTH: usize = 128;

/// 왜 읽지 못했나(사용자에게는 고정 문구만 보인다)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reject {
    /// JSON이 아니다(문법·UTF-8·뒤따르는 글·중첩 한도)
    Syntax,
    /// 같은 객체에 같은 키가 두 번 있다
    DuplicateKey,
}

/// 본문 전체를 한 JSON 값으로 읽는다.
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
            r#""한글 A""#,
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
        // 다른 객체의 같은 키는 괜찮다
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
        // 너무 깊은 중첩은 스택을 쓰지 않고 거절한다
        let deep = format!("{}1{}", "[".repeat(5000), "]".repeat(5000));
        assert_eq!(parse(deep.as_bytes()), Err(Reject::Syntax));
    }
}
