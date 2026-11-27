//! Argument sealing: strict JSON parse, JCS canonicalization, schema check, destination extraction, and SHA-256.

use crate::{
    canon::{canonicalize, sha256},
    json::parse,
    ports::{Sealed, Sealer},
};
use bot_core::{
    command::JsonPayload,
    port::Error,
    profile::{Destination, InputValidator, Tool},
};
use serde_json::Value;
use std::sync::Arc;

pub const MAX_ARGS_BYTES: usize = 256 * 1024;

pub struct JcsSealer {
    validator: Arc<dyn InputValidator>,
}

impl JcsSealer {
    /// `validator` must match the schema validator used during profile activation.
    pub fn new(validator: Arc<dyn InputValidator>) -> Self {
        Self { validator }
    }
}

impl Sealer for JcsSealer {
    fn seal(&self, tool: &Tool, args: &JsonPayload) -> Result<Sealed, Error> {
        if args.0.len() > MAX_ARGS_BYTES {
            return Err(Error::Invalid);
        }
        let value = parse(&args.0).ok_or(Error::Invalid)?;
        if !value.is_object() {
            return Err(Error::Invalid);
        }
        let canonical = canonicalize(&value).ok_or(Error::Invalid)?;
        // Validate the exact canonical bytes that will be passed to the tool.
        self.validator
            .validate(&tool.input, &JsonPayload(canonical.clone()))?;
        let destination = match &tool.destination {
            Some(Destination::Explicit { recipient, content }) => Some((
                text_at(&value, recipient)
                    .filter(|r| !r.is_empty())
                    .ok_or(Error::Invalid)?,
                text_at(&value, content).ok_or(Error::Invalid)?,
            )),
            _ => None,
        };
        let sha256 = sha256(&canonical);
        Ok(Sealed {
            canonical,
            sha256,
            destination,
        })
    }
}

/// Resolves a dotted `args.a.b` string path, returning `None` if missing or non-string.
fn text_at(value: &Value, path: &str) -> Option<String> {
    let mut current = value;
    for key in path.strip_prefix("args.")?.split('.') {
        current = current.as_object()?.get(key)?;
    }
    current.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tool;
    use bot_core::state::Risk;
    use std::sync::Mutex;

    /// Records `(schema, args)` pairs and rejects arguments containing `reject`.
    #[derive(Default)]
    struct Probe {
        seen: Mutex<Vec<(String, String)>>,
        reject: Option<&'static str>,
    }

    impl InputValidator for Probe {
        fn validate(&self, schema: &JsonPayload, args: &JsonPayload) -> Result<(), Error> {
            self.seen
                .lock()
                .unwrap()
                .push((schema.0.clone(), args.0.clone()));
            match self.reject {
                Some(marker) if args.0.contains(marker) => Err(Error::Invalid),
                _ => Ok(()),
            }
        }
    }

    fn sealer(probe: &Arc<Probe>) -> JcsSealer {
        JcsSealer::new(probe.clone())
    }

    fn seal(sealer: &JcsSealer, t: &Tool, args: &str) -> Result<Sealed, Error> {
        sealer.seal(t, &JsonPayload(args.into()))
    }

    fn explicit() -> Tool {
        let mut t = tool(Risk::Write, &["mail.send"]);
        t.destination = Some(Destination::Explicit {
            recipient: "args.to".into(),
            content: "args.message.body".into(),
        });
        t
    }

    #[test]
    fn equivalent_spellings_seal_to_one_hash_and_changes_do_not() {
        let probe = Arc::new(Probe::default());
        let s = sealer(&probe);
        let t = tool(Risk::Read, &["a"]);
        let a = seal(&s, &t, r#"{"b":[1.0,2],"a":"x"}"#).unwrap();
        let b = seal(&s, &t, r#" {"a":"x","b":[1,2.0]} "#).unwrap();
        assert_eq!(a.canonical, r#"{"a":"x","b":[1,2]}"#);
        assert_eq!(
            (a.canonical.as_str(), a.sha256),
            (b.canonical.as_str(), b.sha256)
        );
        assert_ne!(
            a.sha256,
            seal(&s, &t, r#"{"a":"y","b":[1,2]}"#).unwrap().sha256
        );
    }

    #[test]
    fn the_validator_sees_the_exact_bytes_that_will_run_and_the_declared_schema() {
        let probe = Arc::new(Probe::default());
        let s = sealer(&probe);
        let t = tool(Risk::Read, &["a"]);
        let sealed = seal(&s, &t, r#"{ "z" : 1, "a" : 2 }"#).unwrap();
        let seen = probe.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], (t.input.0.clone(), sealed.canonical.clone()));
    }

    #[test]
    fn schema_rejections_pass_through() {
        let probe = Arc::new(Probe {
            reject: Some("forbidden"),
            ..Probe::default()
        });
        let s = sealer(&probe);
        let t = tool(Risk::Read, &["a"]);
        assert!(matches!(
            seal(&s, &t, r#"{"forbidden":1}"#),
            Err(Error::Invalid)
        ));
        assert!(seal(&s, &t, r#"{"ok":1}"#).is_ok());
    }

    #[test]
    fn ambiguous_or_unsafe_documents_are_invalid() {
        let probe = Arc::new(Probe::default());
        let s = sealer(&probe);
        let t = tool(Risk::Read, &["a"]);
        for bad in [
            r#"{"a":1,"a":2}"#,
            r#"[1,2]"#,
            r#""text""#,
            r#"{"id":9007199254740993}"#,
            r#"{"a":1} {"b":2}"#,
            "",
            "not json",
        ] {
            assert!(matches!(seal(&s, &t, bad), Err(Error::Invalid)), "{bad}");
        }
        let big = format!("{{\"a\":\"{}\"}}", "x".repeat(MAX_ARGS_BYTES));
        assert!(matches!(seal(&s, &t, &big), Err(Error::Invalid)));
        assert!(
            probe.seen.lock().unwrap().is_empty(),
            "validator only sees parsed, canonicalized documents"
        );
    }

    #[test]
    fn explicit_destinations_are_read_from_the_declared_paths() {
        let probe = Arc::new(Probe::default());
        let s = sealer(&probe);
        let t = explicit();
        let sealed = seal(
            &s,
            &t,
            r#"{"to":"bob@example.test","message":{"body":"hi\nthere"}}"#,
        )
        .unwrap();
        assert_eq!(
            sealed.destination,
            Some(("bob@example.test".to_string(), "hi\nthere".to_string()))
        );
        // Empty content string still counts as an explicit destination.
        assert!(seal(&s, &t, r#"{"to":"bob","message":{"body":""}}"#).is_ok());
    }

    #[test]
    fn a_destination_that_cannot_be_determined_is_refused() {
        let probe = Arc::new(Probe::default());
        let s = sealer(&probe);
        let t = explicit();
        for bad in [
            r#"{"message":{"body":"hi"}}"#,
            r#"{"to":"","message":{"body":"hi"}}"#,
            r#"{"to":5,"message":{"body":"hi"}}"#,
            r#"{"to":["bob"],"message":{"body":"hi"}}"#,
            r#"{"to":"bob","message":"hi"}"#,
            r#"{"to":"bob","message":{"body":5}}"#,
            r#"{"to":"bob"}"#,
        ] {
            assert!(matches!(seal(&s, &t, bad), Err(Error::Invalid)), "{bad}");
        }
        let mut wrong = explicit();
        wrong.destination = Some(Destination::Explicit {
            recipient: "to".into(),
            content: "args.m".into(),
        });
        assert!(matches!(
            seal(&s, &wrong, r#"{"to":"bob","m":"x"}"#),
            Err(Error::Invalid)
        ));
    }

    #[test]
    fn tools_without_a_destination_or_with_reply_have_none() {
        let probe = Arc::new(Probe::default());
        let s = sealer(&probe);
        let mut reply = tool(Risk::Write, &["chat.post"]);
        reply.destination = Some(Destination::Reply);
        assert!(
            seal(&s, &reply, r#"{"text":"hi"}"#)
                .unwrap()
                .destination
                .is_none()
        );
        assert!(
            seal(&s, &tool(Risk::Read, &["a"]), "{}")
                .unwrap()
                .destination
                .is_none()
        );
    }
}
