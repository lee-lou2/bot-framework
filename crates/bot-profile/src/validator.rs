//! `bot_core::profile::InputValidator` 구현. 선언 검증과 실제 도구 호출이 같은 스키마 해석기를 쓴다.
use crate::schema::Schema;
use bot_core::{command::JsonPayload, port::Error, profile::InputValidator};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

const CACHE_LIMIT: usize = 256;

/// 스키마 문서(정규 JSON 문자열)별로 컴파일 결과를 캐시한다.
#[derive(Default)]
pub struct SchemaValidator {
    cache: Mutex<HashMap<String, Arc<Schema>>>,
}

impl SchemaValidator {
    pub fn new() -> Self {
        Self::default()
    }

    fn compiled(&self, schema: &str) -> Result<Arc<Schema>, Error> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(found) = cache.get(schema) {
            return Ok(found.clone());
        }
        let doc: Value = bot_core::json::parse(schema).ok_or(Error::Invalid)?;
        let compiled = Arc::new(Schema::compile_input(doc).map_err(|_| Error::Invalid)?);
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(schema.to_owned(), compiled.clone());
        Ok(compiled)
    }
}

impl InputValidator for SchemaValidator {
    /// 스키마가 지원 어휘 밖이거나 인자가 스키마에 맞지 않으면 `Invalid`.
    fn validate(&self, schema: &JsonPayload, args: &JsonPayload) -> Result<(), Error> {
        let compiled = self.compiled(&schema.0)?;
        let value: Value = bot_core::json::parse(&args.0).ok_or(Error::Invalid)?;
        if compiled.validate(&value).is_empty() {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(text: &str) -> JsonPayload {
        JsonPayload(text.to_owned())
    }

    #[test]
    fn validates_args_against_the_declared_schema() {
        let v = SchemaValidator::new();
        let schema = payload(
            r#"{"type":"object","required":["event"],"properties":{"event":{"type":"string","pattern":"^[a-z0-9-]+$"}},"additionalProperties":false}"#,
        );
        assert!(
            v.validate(&schema, &payload(r#"{"event":"walk-1"}"#))
                .is_ok()
        );
        for bad in [
            r#"{}"#,
            r#"{"event":1}"#,
            r#"{"event":"Bad Id"}"#,
            r#"{"event":"a","x":1}"#,
            "not json",
            "[]",
        ] {
            assert!(
                matches!(v.validate(&schema, &payload(bad)), Err(Error::Invalid)),
                "{bad}"
            );
        }
        assert!(
            v.validate(&schema, &payload(r#"{"event":"x"}"#)).is_ok(),
            "캐시를 거쳐도 같다"
        );
    }

    #[test]
    fn unsupported_schemas_are_invalid_not_ignored() {
        let v = SchemaValidator::new();
        for schema in [
            r##"{"type":"object","uniqueItems":true}"##,
            r##"{"$ref":"#"}"##,
            r##"{"type":"string","pattern":"\\d+"}"##,
            "nope",
        ] {
            assert!(
                matches!(
                    v.validate(&payload(schema), &payload("{}")),
                    Err(Error::Invalid)
                ),
                "{schema}"
            );
        }
    }

    #[test]
    fn duplicate_keys_and_excessive_depth_are_rejected_in_schema_and_args() {
        let v = SchemaValidator::new();
        let schema = payload(r#"{"type":"object","properties":{"a":{"type":"integer"}}}"#);
        // 인자의 중복 키와 깊이 초과를 거절한다
        assert!(matches!(
            v.validate(&schema, &payload(r#"{"a":1,"a":2}"#)),
            Err(Error::Invalid)
        ));
        let deep = format!(
            "{}1{}",
            "[".repeat(bot_core::json::MAX_DEPTH + 1),
            "]".repeat(bot_core::json::MAX_DEPTH + 1)
        );
        let any_schema = payload(r#"{"type":"object"}"#);
        assert!(matches!(
            v.validate(&any_schema, &payload(&format!(r#"{{"a":{deep}}}"#))),
            Err(Error::Invalid)
        ));
        // 스키마 자체의 중복 키도 거절한다
        let dup_schema = payload(r#"{"type":"object","type":"object"}"#);
        assert!(matches!(
            v.validate(&dup_schema, &payload("{}")),
            Err(Error::Invalid)
        ));
    }
}
