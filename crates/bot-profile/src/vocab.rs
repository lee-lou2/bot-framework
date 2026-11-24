//! 스키마 어휘 검사. 지원하는 키워드만 쓰였는지, 모양이 맞는지, `pattern`이 공통 문법인지 본다.
//! 하나라도 어긋나면 활성화를 거절한다(일부만 검증하고 통과시키지 않는다).
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::HashMap;

/// 번들 스키마만 쓰는 "이 값 자체가 입력 스키마여야 한다" 참조(tools.schema.json의 input).
pub const META_REF: &str = "https://json-schema.org/draft/2020-12/schema";
pub const MAX_DEPTH: usize = 64;
const MAX_PATTERN: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dialect {
    /// 번들한 스키마(schemas/v1)가 쓰는 어휘
    Bundled,
    /// 도구 `input`: docs/plugins.md가 정한 어휘만
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
];
const BUNDLED_EXTRA: &[&str] = &[
    "$schema",
    "$comment",
    "$id",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
    "format",
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

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

pub fn check(doc: &Value, dialect: Dialect) -> Result<HashMap<String, Regex>, Vec<String>> {
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
    errors: Vec<String>,
}

impl Walk<'_> {
    fn bad(&mut self, ptr: &str, msg: impl AsRef<str>) {
        let at = if ptr.is_empty() { "/" } else { ptr };
        self.errors.push(format!("{at}: {}", msg.as_ref()));
    }

    fn allowed(&self, key: &str) -> bool {
        INPUT_KEYS.contains(&key)
            || (self.dialect == Dialect::Bundled && BUNDLED_EXTRA.contains(&key))
    }

    fn node(&mut self, node: &Value, ptr: &str, depth: usize) {
        let obj = match node {
            Value::Bool(_) if self.dialect == Dialect::Bundled || depth > 0 => return,
            Value::Object(o) => o,
            _ => return self.bad(ptr, "스키마는 객체여야 해요"),
        };
        if depth > MAX_DEPTH {
            return self.bad(ptr, "스키마가 너무 깊어요");
        }
        let mut subs: Vec<(String, &Value)> = Vec::new();
        for (key, value) in obj {
            if !self.allowed(key) {
                self.bad(ptr, format!("지원하지 않는 키워드 `{key}`"));
                continue;
            }
            let at = |name: &str| format!("{ptr}/{name}");
            match key.as_str() {
                "type" => self.type_keyword(value, ptr),
                "enum" if !value.is_array() => self.bad(ptr, "enum은 배열이어야 해요"),
                "required"
                    if !value
                        .as_array()
                        .is_some_and(|a| a.iter().all(Value::is_string)) =>
                {
                    self.bad(ptr, "required는 문자열 배열이어야 해요");
                }
                "uniqueItems" if !value.is_boolean() => {
                    self.bad(ptr, "uniqueItems는 true/false여야 해요")
                }
                "minProperties" | "maxProperties" | "minItems" | "maxItems" | "minLength"
                | "maxLength"
                    if value.as_u64().is_none() =>
                {
                    self.bad(ptr, format!("{key}는 0 이상의 정수여야 해요"));
                }
                "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
                    if value.as_f64().is_none() =>
                {
                    self.bad(ptr, format!("{key}는 숫자여야 해요"));
                }
                "multipleOf" if value.as_f64().is_none_or(|m| m <= 0.0) => {
                    self.bad(ptr, "multipleOf는 0보다 큰 숫자여야 해요");
                }
                "pattern" => self.pattern(value, ptr),
                "$ref" => self.reference(value, ptr),
                "properties" | "$defs" | "definitions" => match value.as_object() {
                    Some(map) => subs.extend(
                        map.iter()
                            .map(|(k, v)| (at(&format!("{key}/{}", escape(k))), v)),
                    ),
                    None => self.bad(ptr, format!("{key}는 객체여야 해요")),
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
                    _ => self.bad(ptr, format!("{key}는 비어 있지 않은 배열이어야 해요")),
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
            self.bad(ptr, format!("type이 올바르지 않아요: {value}"));
        }
    }

    fn reference(&mut self, value: &Value, ptr: &str) {
        match value.as_str() {
            Some(META_REF) if self.dialect == Dialect::Bundled => {}
            Some(r) if r.starts_with('#') && self.root.pointer(&r[1..]).is_some() => {}
            Some(r) => self.bad(
                ptr,
                format!("참조할 수 없어요(같은 문서의 `#/…`만 가능): {r}"),
            ),
            None => self.bad(ptr, "$ref는 문자열이어야 해요"),
        }
    }

    fn pattern(&mut self, value: &Value, ptr: &str) {
        let Some(p) = value.as_str() else {
            return self.bad(ptr, "pattern은 문자열이어야 해요");
        };
        if self.patterns.contains_key(p) {
            return;
        }
        if let Err(why) = common_syntax(p) {
            return self.bad(ptr, format!("pattern `{p}`: {why}"));
        }
        match RegexBuilder::new(p).size_limit(1 << 18).build() {
            Ok(re) => {
                self.patterns.insert(p.to_owned(), re);
            }
            Err(e) => self.bad(ptr, format!("pattern `{p}`을 쓸 수 없어요: {e}")),
        }
    }
}

/// Rust regex와 다른 엔진(ECMAScript·PCRE·Python)이 같은 뜻으로 읽는 문법만 허용한다.
/// 글자 모음·경계 이스케이프(`\d \w \s \b`)는 엔진마다 유니코드 범위가 달라 막고 명시적 클래스를 쓰게 한다.
fn common_syntax(p: &str) -> Result<(), String> {
    if p.chars().count() > MAX_PATTERN {
        return Err(format!("{MAX_PATTERN}자를 넘어요"));
    }
    let c: Vec<char> = p.chars().collect();
    let (mut i, mut in_class) = (0, false);
    while i < c.len() {
        match (c[i], in_class) {
            ('\\', _) => {
                let next = *c.get(i + 1).ok_or("끝이 `\\`로 끝나요")?;
                let ok = if in_class {
                    "\\-][^./nrt"
                } else {
                    "\\.-/^$*+?()[]{}|nrt"
                };
                if !ok.contains(next) {
                    let hint = if "dDwWsSbB".contains(next) {
                        "(`[0-9]`처럼 명시적 클래스를 쓰세요)"
                    } else {
                        ""
                    };
                    return Err(format!("`\\{next}`는 공통 문법이 아니에요{hint}"));
                }
                i += 1;
            }
            ('(', false) if c.get(i + 1) == Some(&'?') && c.get(i + 2) != Some(&':') => {
                return Err(
                    "`(?:` 말고 `(?`로 시작하는 그룹(플래그·lookaround·이름)은 쓸 수 없어요".into(),
                );
            }
            ('[', false) => {
                in_class = true;
                if c.get(i + 1) == Some(&'^') {
                    i += 1;
                }
                if c.get(i + 1) == Some(&']') {
                    return Err("`[]`·`[^]`처럼 `]`로 시작하는 클래스는 엔진마다 달라요".into());
                }
            }
            ('[', true) => return Err("클래스 안의 `[`는 쓸 수 없어요(`\\[`로 쓰세요)".into()),
            (']', true) => in_class = false,
            ('&' | '~' | '-', true) if c.get(i + 1) == Some(&c[i]) => {
                return Err("클래스 안의 집합 연산(`&&` `--` `~~`)은 쓸 수 없어요".into());
            }
            _ => {}
        }
        i += 1;
    }
    if in_class {
        Err("`[`가 닫히지 않았어요".into())
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
        for key in [
            "$schema",
            "$ref",
            "$defs",
            "uniqueItems",
            "format",
            "not",
            "if",
            "patternProperties",
            "propertyNames",
            "exclusiveMinimum",
            "multipleOf",
            "$id",
            "examples",
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
        assert!(input(json!(true)).is_err(), "최상위 스키마는 객체여야 해요");
    }
}
