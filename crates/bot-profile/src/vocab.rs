//! JSON Schema vocabulary, shape, and portable regex syntax checks for bundled and tool input schemas.
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::HashMap;

/// Bundled-only meta-schema reference validating that a property value is itself a supported input schema.
pub const META_REF: &str = "https://json-schema.org/draft/2020-12/schema";
pub const MAX_DEPTH: usize = 64;
const MAX_PATTERN: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dialect {
    Bundled,
    Input,
}

const INPUT_KEYS: &[&str] = &[
    "type",
    "enum",
    "const",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "pattern",
    "anyOf",
    "oneOf",
    "allOf",
    "title",
    "description",
    "default",
    // Annotation keywords accepted without enforcing them during validation (common in MCP tool schemas).
    "$schema",
    "$comment",
    "examples",
    "format",
    "deprecated",
    "readOnly",
    "writeOnly",
    // MCP Streamable HTTP annotation forwarding the argument as `Mcp-Param-<name>`; value must be an HTTP header token.
    "x-mcp-header",
];
const BUNDLED_EXTRA: &[&str] = &[
    "$id",
    "$ref",
    "$defs",
    "definitions",
    "propertyNames",
    "minProperties",
    "maxProperties",
    "uniqueItems",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "not",
    "if",
    "then",
    "else",
];
const TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "string", "number", "integer",
];

/// RFC 9110 `token` (`tchar`), 1..=64 ASCII bytes.
fn header_token(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Vocabulary violation with stable `code` and JSON `pointer`; `message` may echo schema text,
/// so diagnostics for untrusted server schemas must use `code` and `pointer` only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Problem {
    pub code: &'static str,
    pub pointer: String,
    pub message: String,
}

impl Problem {
    pub fn line(&self) -> String {
        let at = if self.pointer.is_empty() {
            "/"
        } else {
            &self.pointer
        };
        format!("{at}: {}", self.message)
    }
}

pub fn check(doc: &Value, dialect: Dialect) -> Result<HashMap<String, Regex>, Vec<String>> {
    check_problems(doc, dialect).map_err(|found| found.iter().map(Problem::line).collect())
}

pub fn check_problems(
    doc: &Value,
    dialect: Dialect,
) -> Result<HashMap<String, Regex>, Vec<Problem>> {
    let mut walk = Walk {
        root: doc,
        dialect,
        patterns: HashMap::new(),
        errors: Vec::new(),
    };
    walk.node(doc, "", 0);
    if walk.errors.is_empty() {
        Ok(walk.patterns)
    } else {
        Err(walk.errors)
    }
}

struct Walk<'a> {
    root: &'a Value,
    dialect: Dialect,
    patterns: HashMap<String, Regex>,
    errors: Vec<Problem>,
}

impl Walk<'_> {
    fn bad(&mut self, ptr: &str, code: &'static str, msg: impl AsRef<str>) {
        self.errors.push(Problem {
            code,
            pointer: ptr.to_owned(),
            message: msg.as_ref().to_owned(),
        });
    }

    fn allowed(&self, key: &str) -> bool {
        INPUT_KEYS.contains(&key)
            || (self.dialect == Dialect::Bundled && BUNDLED_EXTRA.contains(&key))
    }

    fn node(&mut self, node: &Value, ptr: &str, depth: usize) {
        let obj = match node {
            Value::Bool(_) if self.dialect == Dialect::Bundled || depth > 0 => return,
            Value::Object(o) => o,
            _ => return self.bad(ptr, "shape", "schema must be an object"),
        };
        if depth > MAX_DEPTH {
            return self.bad(ptr, "too-deep", "schema is too deeply nested");
        }
        let mut subs: Vec<(String, &Value)> = Vec::new();
        for (key, value) in obj {
            if !self.allowed(key) {
                self.bad(
                    ptr,
                    "unsupported-keyword",
                    format!("unsupported keyword `{key}`"),
                );
                continue;
            }
            let at = |name: &str| format!("{ptr}/{name}");
            match key.as_str() {
                "type" => self.type_keyword(value, ptr),
                "enum" if !value.is_array() => self.bad(ptr, "shape", "enum must be an array"),
                "required"
                    if !value
                        .as_array()
                        .is_some_and(|a| a.iter().all(Value::is_string)) =>
                {
                    self.bad(ptr, "shape", "required must be an array of strings");
                }
                "uniqueItems" if !value.is_boolean() => {
                    self.bad(ptr, "shape", "uniqueItems must be a boolean")
                }
                "minProperties" | "maxProperties" | "minItems" | "maxItems" | "minLength"
                | "maxLength"
                    if value.as_u64().is_none() =>
                {
                    self.bad(
                        ptr,
                        "shape",
                        format!("{key} must be a non-negative integer"),
                    );
                }
                "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
                    if value.as_f64().is_none() =>
                {
                    self.bad(ptr, "shape", format!("{key} must be a number"));
                }
                "multipleOf" if value.as_f64().is_none_or(|m| m <= 0.0) => {
                    self.bad(ptr, "shape", "multipleOf must be a number greater than 0");
                }
                "pattern" => self.pattern(value, ptr),
                "$schema" | "$comment" | "format" if !value.is_string() => {
                    self.bad(ptr, "shape", format!("{key} must be a string"));
                }
                "examples" if !value.is_array() => {
                    self.bad(ptr, "shape", "examples must be an array")
                }
                "deprecated" | "readOnly" | "writeOnly" if !value.is_boolean() => {
                    self.bad(ptr, "shape", format!("{key} must be a boolean"));
                }
                "x-mcp-header" if !value.as_str().is_some_and(header_token) => {
                    self.bad(
                        ptr,
                        "bad-header-name",
                        "x-mcp-header must be a valid HTTP header token (1..=64 RFC 9110 token chars)",
                    );
                }
                "$ref" => self.reference(value, ptr),
                "properties" | "$defs" | "definitions" => match value.as_object() {
                    Some(map) => subs.extend(
                        map.iter()
                            .map(|(k, v)| (at(&format!("{key}/{}", escape(k))), v)),
                    ),
                    None => self.bad(ptr, "shape", format!("{key} must be an object")),
                },
                "additionalProperties"
                | "propertyNames"
                | "items"
                | "not"
                | "if"
                | "then"
                | "else" => {
                    subs.push((at(key), value));
                }
                "allOf" | "anyOf" | "oneOf" => match value.as_array() {
                    Some(list) if !list.is_empty() => {
                        subs.extend(
                            list.iter()
                                .enumerate()
                                .map(|(i, v)| (at(&format!("{key}/{i}")), v)),
                        );
                    }
                    _ => self.bad(ptr, "shape", format!("{key} must be a non-empty array")),
                },
                _ => {}
            }
        }
        for (sub_ptr, sub) in subs {
            self.node(sub, &sub_ptr, depth + 1);
        }
    }

    fn type_keyword(&mut self, value: &Value, ptr: &str) {
        let names: Vec<&str> = match value {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        let count = if let Value::Array(a) = value {
            a.len()
        } else {
            1
        };
        if names.is_empty() || names.len() != count || names.iter().any(|n| !TYPES.contains(n)) {
            self.bad(ptr, "bad-type", format!("invalid type: {value}"));
        }
    }

    fn reference(&mut self, value: &Value, ptr: &str) {
        match value.as_str() {
            Some(META_REF) if self.dialect == Dialect::Bundled => {}
            Some(r) if r.starts_with('#') && self.root.pointer(&r[1..]).is_some() => {}
            Some(r) => self.bad(
                ptr,
                "bad-ref",
                format!("unresolvable $ref (only local `#/...` pointers are allowed): {r}"),
            ),
            None => self.bad(ptr, "bad-ref", "$ref must be a string"),
        }
    }

    fn pattern(&mut self, value: &Value, ptr: &str) {
        let Some(p) = value.as_str() else {
            return self.bad(ptr, "bad-pattern", "pattern must be a string");
        };
        if self.patterns.contains_key(p) {
            return;
        }
        if let Err(why) = common_syntax(p) {
            return self.bad(ptr, "bad-pattern", format!("pattern `{p}`: {why}"));
        }
        match RegexBuilder::new(p).size_limit(1 << 18).build() {
            Ok(re) => {
                self.patterns.insert(p.to_owned(), re);
            }
            Err(e) => self.bad(ptr, "bad-pattern", format!("invalid pattern `{p}`: {e}")),
        }
    }
}

/// Restricts regexes to syntax portable across Rust `regex`, ECMAScript, PCRE, and Python
/// (rejecting `\d`, `\w`, `\s`, `\b` whose Unicode ranges vary across engines).
fn common_syntax(p: &str) -> Result<(), String> {
    if p.chars().count() > MAX_PATTERN {
        return Err(format!("exceeds {MAX_PATTERN} characters"));
    }
    let c: Vec<char> = p.chars().collect();
    let (mut i, mut in_class) = (0, false);
    while i < c.len() {
        match (c[i], in_class) {
            ('\\', _) => {
                let next = *c.get(i + 1).ok_or("trailing `\\` escape")?;
                let ok = if in_class {
                    "\\-][^./nrt"
                } else {
                    "\\.-/^$*+?()[]{}|nrt"
                };
                if !ok.contains(next) {
                    let hint = if "dDwWsSbB".contains(next) {
                        " (use an explicit class such as `[0-9]`)"
                    } else {
                        ""
                    };
                    return Err(format!("`\\{next}` is not portable regex syntax{hint}"));
                }
                i += 1;
            }
            ('(', false) if c.get(i + 1) == Some(&'?') && c.get(i + 2) != Some(&':') => {
                return Err(
                    "`(?` groups other than `(?:` (flags, lookarounds, named groups) are not allowed".into(),
                );
            }
            ('[', false) => {
                in_class = true;
                if c.get(i + 1) == Some(&'^') {
                    i += 1;
                }
                if c.get(i + 1) == Some(&']') {
                    return Err(
                        "character classes starting with `]` (`[]`, `[^]`) differ across regex engines".into(),
                    );
                }
            }
            ('[', true) => {
                return Err(
                    "unescaped `[` inside character class is not allowed (use `\\[`)".into(),
                );
            }
            (']', true) => in_class = false,
            ('&' | '~' | '-', true) if c.get(i + 1) == Some(&c[i]) => {
                return Err(
                    "character class set operators (`&&`, `--`, `~~`) are not allowed".into(),
                );
            }
            _ => {}
        }
        i += 1;
    }
    if in_class {
        Err("unclosed `[` in character class".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(doc: Value) -> Result<(), Vec<String>> {
        check(&doc, Dialect::Input).map(|_| ())
    }

    #[test]
    fn input_vocabulary_is_the_documented_one() {
        let ok = json!({"type": "object", "title": "t", "description": "d", "default": {},
            "properties": {"a": {"type": "string", "minLength": 1, "maxLength": 5, "pattern": "^[a-z]+$", "enum": ["x"]},
                           "n": {"type": "integer", "minimum": 0, "maximum": 9},
                           "l": {"type": "array", "items": {"const": 1}, "minItems": 0, "maxItems": 2},
                           "o": {"anyOf": [{"type": "null"}, {"type": "string"}]}},
            "required": ["a"], "additionalProperties": false, "allOf": [{"required": ["a"]}], "oneOf": [{"type": "object"}]});
        assert!(input(ok).is_ok());
        // `$ref`/`$defs` are expanded before vocabulary checks by `Schema::compile_input`, so raw vocab rejects them.
        for key in [
            "$ref",
            "$defs",
            "definitions",
            "uniqueItems",
            "not",
            "if",
            "patternProperties",
            "propertyNames",
            "exclusiveMinimum",
            "multipleOf",
            "$id",
            "$anchor",
            "$dynamicRef",
            "dependentSchemas",
        ] {
            let doc =
                json!({"type": "object", key: if key == "$ref" { json!("#") } else { json!({}) }});
            assert!(input(doc).is_err(), "{key}");
        }
        assert!(
            check(
                &json!({"propertyNames": {"pattern": "^a$"}, "uniqueItems": true, "not": false}),
                Dialect::Bundled
            )
            .is_ok()
        );
    }

    #[test]
    fn patterns_use_common_syntax_only() {
        for ok in [
            "^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$",
            "^/[^?#]*$",
            "^args\\.[A-Za-z_][A-Za-z0-9_]*$",
            "^[0-9]{1,5}$",
            "a|b",
            "^[a-z0-9.-]+:[0-9]{1,5}$",
        ] {
            assert!(common_syntax(ok).is_ok(), "{ok}");
        }
        for bad in [
            "\\d+",
            "\\w",
            "\\bfoo",
            "(?i)a",
            "(?<n>a)",
            "(?=a)",
            "[[:alpha:]]",
            "[a-z&&[^b]]",
            "[]a]",
            "(?P<x>a)",
            "\\p{L}",
            "[a",
            &"a".repeat(257),
            "\\u{41}",
            "\\x41",
        ] {
            assert!(common_syntax(bad).is_err(), "{bad}");
        }
        assert!(input(json!({"type": "string", "pattern": "("})).is_err());
        assert!(input(json!({"type": "string", "pattern": "\\d"})).is_err());
    }

    #[test]
    fn shape_errors_are_reported() {
        for doc in [
            json!({"type": "nope"}),
            json!({"required": "a"}),
            json!({"anyOf": []}),
            json!({"minLength": -1}),
            json!(5),
            json!({"properties": []}),
        ] {
            assert!(input(doc.clone()).is_err(), "{doc}");
        }
        assert!(
            input(json!(true)).is_err(),
            "top-level schema must be an object"
        );
    }

    #[test]
    fn annotation_keywords_are_accepted_but_never_checked_against_the_value() {
        let doc = json!({
            "$schema": "http://json-schema.org/draft-07/schema#", "$comment": "generated schema",
            "type": "object", "title": "t", "description": "d", "examples": [{"a": "x"}],
            "properties": {
                "a": {"type": "string", "format": "uri", "deprecated": true, "readOnly": false,
                      "writeOnly": false, "examples": ["x"], "default": "y", "$comment": "c"}
            }
        });
        assert!(input(doc).is_ok());
        for bad in [
            json!({"type": "object", "$schema": {}}),
            json!({"type": "object", "format": 1}),
            json!({"type": "object", "examples": {}}),
            json!({"type": "object", "deprecated": "yes"}),
            json!({"type": "object", "readOnly": 1}),
        ] {
            assert!(input(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_mcp_header_annotation_takes_a_field_name_token_only() {
        let with = |value: Value| {
            json!({"type": "object",
                   "properties": {"region": {"type": "string", "x-mcp-header": value}}})
        };
        assert!(input(with(json!("Region"))).is_ok());
        assert!(input(with(json!("X-Region_1"))).is_ok());
        for bad in [
            json!(""),
            json!("a b"),
            json!("a:b"),
            json!("é"),
            json!(5),
            json!(null),
            json!("x".repeat(65)),
        ] {
            assert!(input(with(bad.clone())).is_err(), "{bad}");
        }
        // Other `x-` extension keys remain outside the vocabulary.
        assert!(input(json!({"type": "object", "x-other": 1})).is_err());
    }
}
