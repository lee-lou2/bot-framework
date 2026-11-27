//! Scans raw and nested JSON-decoded upstream responses for the request's provider secret key.
//!
//! Checks the raw key plus 1x and 2x JSON-escaped forms across raw response bytes, recursively
//! decoded JSON strings/keys, and the parsed `ModelStep`; any hit fails closed without redacting.

use crate::model::ModelStep;
use serde_json::Value;

const MAX_NEST: usize = 4;

pub(super) struct Needles(Vec<String>);

impl Needles {
    pub(super) fn new(key: &str) -> Self {
        let once = escaped(key);
        let twice = escaped(&once);
        let mut forms = vec![key.to_owned()];
        for form in [once, twice] {
            if !forms.contains(&form) {
                forms.push(form);
            }
        }
        Self(forms)
    }

    fn in_text(&self, text: &str) -> bool {
        self.0.iter().any(|needle| text.contains(needle.as_str()))
    }

    fn in_string(&self, text: &str, depth: usize) -> bool {
        if self.in_text(text) {
            return true;
        }
        let trimmed = text.trim_start();
        depth < MAX_NEST
            && trimmed.starts_with(['{', '[', '"'])
            && serde_json::from_str::<Value>(text)
                .is_ok_and(|inner| self.in_value(&inner, depth + 1))
    }

    fn in_value(&self, value: &Value, depth: usize) -> bool {
        match value {
            Value::String(text) => self.in_string(text, depth),
            Value::Array(items) => items.iter().any(|item| self.in_value(item, depth)),
            Value::Object(map) => map
                .iter()
                .any(|(name, item)| self.in_text(name) || self.in_value(item, depth)),
            _ => false,
        }
    }

    pub(super) fn in_body(&self, raw: &str) -> bool {
        self.in_text(raw)
            || serde_json::from_str::<Value>(raw).is_ok_and(|value| self.in_value(&value, 0))
    }

    pub(super) fn in_step(&self, step: &ModelStep) -> bool {
        match step {
            ModelStep::Final { text, .. } => self.in_string(text, 0),
            ModelStep::Calls {
                text, calls, state, ..
            } => {
                self.in_string(text, 0)
                    || state
                        .as_deref()
                        .is_some_and(|state| self.in_string(state, 0))
                    || calls.iter().any(|call| self.in_string(&call.args.0, 0))
            }
        }
    }
}

fn escaped(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted
        .get(1..quoted.len().saturating_sub(1))
        .unwrap_or_default()
        .to_owned()
}
