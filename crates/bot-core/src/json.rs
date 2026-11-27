//! 엄격한 JSON 읽기. 중복 키·꼬리 글·과한 깊이를 거절한다.
//! 승인한 바이트와 도구가 받는 바이트를 같게 하려면 읽는 쪽이 모호함을 남기지 않아야 한다
//! (중복 키는 파서마다 첫 값·마지막 값으로 갈린다).

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::fmt;

/// 중첩 깊이 상한.
pub const MAX_DEPTH: usize = 32;

/// 엄격한 JSON 읽기 실패 이유.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StrictError {
    /// 문법 오류·뒤따르는 글·중첩 깊이 초과·비유한 수.
    Syntax,
    /// 같은 객체 안에 같은 키가 두 번 이상 있다.
    DuplicateKey,
}

/// 문서 하나를 `max_depth` 깊이 한도 안에서 엄격하게 읽는다.
pub fn parse_strict_with_depth(input: &str, max_depth: usize) -> Result<Value, StrictError> {
    let mut de = serde_json::Deserializer::from_str(input);
    let mut duplicate = false;
    let value = Strict {
        depth: max_depth,
        duplicate: &mut duplicate,
    }
    .deserialize(&mut de)
    .map_err(|_| {
        if duplicate {
            StrictError::DuplicateKey
        } else {
            StrictError::Syntax
        }
    })?;
    de.end().map_err(|_| StrictError::Syntax)?;
    Ok(value)
}

/// 문서 하나를 읽는다. 중복 키·꼬리 글·깊이 초과·비유한 수는 `None`이다.
pub fn parse(text: &str) -> Option<Value> {
    parse_strict_with_depth(text, MAX_DEPTH).ok()
}

struct Strict<'a> {
    depth: usize,
    duplicate: &'a mut bool,
}

impl<'de> DeserializeSeed<'de> for Strict<'_> {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Strict<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let depth = self
            .depth
            .checked_sub(1)
            .ok_or_else(|| de::Error::custom("nesting too deep"))?;
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(Strict {
            depth,
            duplicate: &mut *self.duplicate,
        })? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let depth = self
            .depth
            .checked_sub(1)
            .ok_or_else(|| de::Error::custom("nesting too deep"))?;
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if out.contains_key(&key) {
                *self.duplicate = true;
                return Err(de::Error::custom("duplicate key"));
            }
            let value = map.next_value_seed(Strict {
                depth,
                duplicate: &mut *self.duplicate,
            })?;
            out.insert(key, value);
        }
        Ok(Value::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ordinary_documents() {
        let value = parse(r#"{"a":[1,2.5,null,true],"b":{"c":"d"}}"#).unwrap();
        assert_eq!(value["a"][1], 2.5);
        assert_eq!(value["b"]["c"], "d");
        assert!(parse("  {} \n").is_some());
    }

    #[test]
    fn duplicate_keys_are_refused_at_any_depth() {
        assert!(parse(r#"{"a":1,"a":2}"#).is_none());
        assert!(parse(r#"{"x":{"a":1,"a":1}}"#).is_none());
        assert!(parse(r#"[{"a":1,"b":1,"a":1}]"#).is_none());
        // 이스케이프로 같아지는 키도 같은 키다
        assert!(parse(r#"{"a":1,"\u0061":2}"#).is_none());
    }

    #[test]
    fn trailing_garbage_and_malformed_text_are_refused() {
        for bad in [
            "{} x",
            "{}{}",
            "{\"a\":1,}",
            "[1,]",
            "{'a':1}",
            "{\"a\":01}",
            "{\"a\":NaN}",
            "",
            "{\"a\":1",
            "\"\\ud800\"",
        ] {
            assert!(parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn nesting_beyond_the_limit_is_refused() {
        let ok = format!("{}1{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        let deep = format!(
            "{}1{}",
            "[".repeat(MAX_DEPTH + 1),
            "]".repeat(MAX_DEPTH + 1)
        );
        assert!(parse(&ok).is_some());
        assert!(parse(&deep).is_none());
    }

    #[test]
    fn numbers_too_large_for_floats_are_not_finite() {
        assert!(parse("1e999").is_none());
    }

    #[test]
    fn parse_strict_with_depth_distinguishes_duplicate_key_from_syntax_and_depth() {
        assert_eq!(
            parse_strict_with_depth(r#"{"a":1,"a":2}"#, 4),
            Err(StrictError::DuplicateKey)
        );
        assert_eq!(
            parse_strict_with_depth(r#"{"a":1,"x":{"k":1,"k":1}}"#, 4),
            Err(StrictError::DuplicateKey)
        );
        assert_eq!(
            parse_strict_with_depth(r#"{"a":1} trailing"#, 4),
            Err(StrictError::Syntax)
        );
        assert_eq!(
            parse_strict_with_depth("[[[[1]]]]", 4),
            Ok(serde_json::json!([[[[1]]]]))
        );
        assert_eq!(
            parse_strict_with_depth("[[[[[1]]]]]", 4),
            Err(StrictError::Syntax)
        );
    }
}
