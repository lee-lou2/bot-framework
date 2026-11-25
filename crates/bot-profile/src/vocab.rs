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
    // 주석 키워드: 받아들이되 검증에는 쓰지 않는다(실제 MCP 서버의 입력 스키마가 흔히 싣는다)
    "$schema",
    "$comment",
    "examples",
    "format",
    "deprecated",
    "readOnly",
    "writeOnly",
    // MCP Streamable HTTP: 이 인자의 값을 `Mcp-Param-<이름>` 머리로 되풀이한다(주석일 뿐 검증에는 영향이 없다). 값은 HTTP 필드 이름 토큰이다.
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

/// RFC 9110 `token`(`tchar`)으로만 이뤄진 1~64자.
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

/// 어휘 검사에서 찾은 문제 하나. `code`는 고정 이름이고 `pointer`는 문제가 있는 스키마 자리(JSON 포인터)다. `message`는 사람이 읽는 설명이며 **스키마 안의 글(키워드·패턴 등)을
/// 되풀이할 수 있다** — 출처를 믿을 수 없는 스키마(서버가 내세운 정의)의 진단에는 `code`와 `pointer`만 쓴다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Problem {
    pub code: &'static str,
    pub pointer: String,
    pub message: String,
}

impl Problem {
    /// 옛 한 줄 모양(`<포인터>: <설명>`)
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
            _ => return self.bad(ptr, "shape", "스키마는 객체여야 해요"),
        };
        if depth > MAX_DEPTH {
            return self.bad(ptr, "too-deep", "스키마가 너무 깊어요");
        }
        let mut subs: Vec<(String, &Value)> = Vec::new();
        for (key, value) in obj {
            if !self.allowed(key) {
                self.bad(
                    ptr,
                    "unsupported-keyword",
                    format!("지원하지 않는 키워드 `{key}`"),
                );
                continue;
            }
            let at = |name: &str| format!("{ptr}/{name}");
            match key.as_str() {
                "type" => self.type_keyword(value, ptr),
                "enum" if !value.is_array() => self.bad(ptr, "shape", "enum은 배열이어야 해요"),
                "required"
                    if !value
                        .as_array()
                        .is_some_and(|a| a.iter().all(Value::is_string)) =>
                {
                    self.bad(ptr, "shape", "required는 문자열 배열이어야 해요");
                }
                "uniqueItems" if !value.is_boolean() => {
                    self.bad(ptr, "shape", "uniqueItems는 true/false여야 해요")
                }
                "minProperties" | "maxProperties" | "minItems" | "maxItems" | "minLength"
                | "maxLength"
                    if value.as_u64().is_none() =>
                {
                    self.bad(ptr, "shape", format!("{key}는 0 이상의 정수여야 해요"));
                }
                "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
                    if value.as_f64().is_none() =>
                {
                    self.bad(ptr, "shape", format!("{key}는 숫자여야 해요"));
                }
                "multipleOf" if value.as_f64().is_none_or(|m| m <= 0.0) => {
                    self.bad(ptr, "shape", "multipleOf는 0보다 큰 숫자여야 해요");
                }
                "pattern" => self.pattern(value, ptr),
                "$schema" | "$comment" | "format" if !value.is_string() => {
                    self.bad(ptr, "shape", format!("{key}는 글이어야 해요"));
                }
                "examples" if !value.is_array() => {
                    self.bad(ptr, "shape", "examples는 배열이어야 해요")
                }
                "deprecated" | "readOnly" | "writeOnly" if !value.is_boolean() => {
                    self.bad(ptr, "shape", format!("{key}는 true/false여야 해요"));
                }
                "x-mcp-header" if !value.as_str().is_some_and(header_token) => {
                    self.bad(ptr, "bad-header-name", "x-mcp-header는 HTTP 필드 이름으로 쓸 수 있는 글자(영숫자와 `-` `_` 등 64자 이하)여야 해요");
                }
                "$ref" => self.reference(value, ptr),
                "properties" | "$defs" | "definitions" => match value.as_object() {
                    Some(map) => subs.extend(
                        map.iter()
                            .map(|(k, v)| (at(&format!("{key}/{}", escape(k))), v)),
                    ),
                    None => self.bad(ptr, "shape", format!("{key}는 객체여야 해요")),
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
                    _ => self.bad(
                        ptr,
                        "shape",
                        format!("{key}는 비어 있지 않은 배열이어야 해요"),
                    ),
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
            self.bad(ptr, "bad-type", format!("type이 올바르지 않아요: {value}"));
        }
    }

    fn reference(&mut self, value: &Value, ptr: &str) {
        match value.as_str() {
            Some(META_REF) if self.dialect == Dialect::Bundled => {}
            Some(r) if r.starts_with('#') && self.root.pointer(&r[1..]).is_some() => {}
            Some(r) => self.bad(
                ptr,
                "bad-ref",
                format!("참조할 수 없어요(같은 문서의 `#/…`만 가능): {r}"),
            ),
            None => self.bad(ptr, "bad-ref", "$ref는 문자열이어야 해요"),
        }
    }

    fn pattern(&mut self, value: &Value, ptr: &str) {
        let Some(p) = value.as_str() else {
            return self.bad(ptr, "bad-pattern", "pattern은 문자열이어야 해요");
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
            Err(e) => self.bad(
                ptr,
                "bad-pattern",
                format!("pattern `{p}`을 쓸 수 없어요: {e}"),
            ),
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
        // `$ref`·`$defs`는 어휘 검사 앞에서 `Schema::compile_input`이 펼친다: 어휘 검사 자체는 남은 `$ref`를 받지 않는다
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
        assert!(input(json!(true)).is_err(), "최상위 스키마는 객체여야 해요");
    }

    #[test]
    fn annotation_keywords_are_accepted_but_never_checked_against_the_value() {
        let doc = json!({
            "$schema": "http://json-schema.org/draft-07/schema#", "$comment": "생성한 스키마",
            "type": "object", "title": "t", "description": "d", "examples": [{"a": "x"}],
            "properties": {
                "a": {"type": "string", "format": "uri", "deprecated": true, "readOnly": false,
                      "writeOnly": false, "examples": ["x"], "default": "y", "$comment": "c"}
            }
        });
        assert!(input(doc).is_ok());
        // 모양이 틀린 주석은 거절한다
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
        // 다른 `x-` 키는 여전히 어휘 밖이다
        assert!(input(json!({"type": "object", "x-other": 1})).is_err());
    }
}
