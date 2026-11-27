//! Implements `bot_core::profile::InputValidator` using the same schema compiler as profile loading.
use crate::schema::Schema;
use bot_core::{command::JsonPayload, port::Error, profile::InputValidator};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

const CACHE_LIMIT: usize = 256;

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
            "cached validation matches"
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
        let dup_schema = payload(r#"{"type":"object","type":"object"}"#);
        assert!(matches!(
            v.validate(&dup_schema, &payload("{}")),
            Err(Error::Invalid)
        ));
    }
}
