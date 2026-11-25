//! 도구 입력 스키마의 로컬 `$ref` 펼치기. 실제 MCP 서버의 입력 스키마(예: 모델 정의에서 생성한 것)는 `$defs`(또는 `definitions`)에 공용 조각을 두고 `$ref`로 쓴다.
//! 컴파일할 때 **재귀 없는** 로컬 참조(`#/$defs/<이름>`·`#/definitions/<이름>`)만 그 자리에 펼치고 `$defs`·`definitions`는 지운다. 그 밖은 활성화·동기화를 거절한다:
//! 재귀(자기 자신이나 서로를 부르는 참조, `#`)·외부 참조·정의가 없는 참조·그 밖의 JSON 포인터·최상위가 아닌 `$defs`. 펼친 결과가 너무 크면(참조가 서로를 여러 번 부르는
//! 지수 증가) 크기 한도에서 멈추고 오류를 낸다.
//!
//! 스키마 구조를 따라간다: 키워드가 가리키는 하위 스키마만 훑고(`properties`의 속성 이름·`enum`·`const`·`default`·`examples` 값은 데이터라 건드리지 않는다),
//! `$ref`가 있는 자리는 정의로 바꾼다. **`$ref` 옆에는 주석성 키워드만 둘 수 있다**(그 밖의 형제는 조건을 바꾸거나 버릴 수 있으니 이유를 밝혀 거절한다: 조건을 조용히
//! 약하게 만드는 길 자체가 없다). `$ref`의 조각은 URI 규칙대로 **퍼센트 부호를 먼저 풀고**(`%20`→공백) 그다음 JSON 포인터 이스케이프(`~1`→`/`, `~0`→`~`)를 푼다.
use crate::vocab::Problem;
use serde_json::{Map, Value};

/// 펼치는 동안 훑는 스키마 노드의 수 한도.
const MAX_NODES: usize = 20_000;
/// 스키마 깊이 한도(vocab과 같다).
const MAX_DEPTH: usize = 64;
/// 참조 사슬(참조가 참조를 부르는 깊이) 한도.
const MAX_CHAIN: usize = 32;

/// `$ref` 옆에 둘 수 있는 키워드: 검증 조건이 아닌 주석(`$schema`·`format`은 이 어휘가 값을 검사하지 않는 메타·주석이다). 정의에 얹는다.
const SIBLINGS: &[&str] = &[
    "title",
    "description",
    "default",
    "examples",
    "$comment",
    "deprecated",
    "readOnly",
    "writeOnly",
    "$schema",
    "format",
];

/// 하위 스키마 하나를 값으로 갖는 키워드.
const ONE: &[&str] = &[
    "items",
    "additionalProperties",
    "propertyNames",
    "not",
    "if",
    "then",
    "else",
];

/// 하위 스키마 배열을 값으로 갖는 키워드.
const MANY: &[&str] = &["allOf", "anyOf", "oneOf"];

struct Expander {
    defs: Map<String, Value>,
    legacy: Map<String, Value>,
    nodes: usize,
    /// 지금 펼치는 중인 정의들(재귀를 알아낸다)
    stack: Vec<String>,
}

fn problem(code: &'static str, pointer: &str, message: impl Into<String>) -> Problem {
    Problem {
        code,
        pointer: pointer.to_owned(),
        message: message.into(),
    }
}

/// JSON 포인터의 한 토막(`~`·`/` 이스케이프).
fn token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// `$ref`를 펼친 스키마. `$ref`·`$defs`가 없으면 그대로 돌려준다. 문제는 고정 코드와 JSON 포인터로 알린다.
pub fn expand(doc: Value) -> Result<Value, Problem> {
    let Value::Object(mut root) = doc else {
        return Ok(doc);
    };
    let mut take = |key: &str| match root.remove(key) {
        None => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map),
        Some(_) => Err(problem(
            "bad-defs",
            &format!("/{key}"),
            format!("`{key}`는 객체여야 해요"),
        )),
    };
    let (defs, legacy) = (take("$defs")?, take("definitions")?);
    let mut walk = Expander {
        defs,
        legacy,
        nodes: 0,
        stack: Vec::new(),
    };
    walk.schema(Value::Object(root), 0, true, "")
}

impl Expander {
    fn count(&mut self, at: &str) -> Result<(), Problem> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(problem(
                "ref-too-large",
                at,
                format!(
                    "$ref를 펼친 스키마가 너무 커요(스키마 노드 {MAX_NODES}개 한도) — 참조가 서로를 여러 번 부르는지 확인하세요"
                ),
            ));
        }
        Ok(())
    }

    fn schema(
        &mut self,
        node: Value,
        depth: usize,
        root: bool,
        at: &str,
    ) -> Result<Value, Problem> {
        let Value::Object(mut map) = node else {
            return Ok(node);
        };
        if depth > MAX_DEPTH {
            return Err(problem("too-deep", at, "스키마가 너무 깊어요"));
        }
        self.count(at)?;
        if !root && (map.contains_key("$defs") || map.contains_key("definitions")) {
            return Err(problem(
                "defs-nested",
                at,
                "`$defs`·`definitions`는 스키마 최상위에만 둘 수 있어요(하위 스키마 안의 정의는 지원하지 않아요)",
            ));
        }
        let reference = match map.remove("$ref") {
            None => None,
            Some(Value::String(text)) => Some(text),
            Some(_) => return Err(problem("bad-ref", at, "$ref는 문자열이어야 해요")),
        };
        let own = self.keywords(map, depth, at)?;
        let Some(reference) = reference else {
            return Ok(Value::Object(own));
        };
        let target = self.resolve(&reference, depth, at)?;
        combine(target, own, at)
    }

    /// 한 스키마 객체의 하위 스키마를 펼친다(그 밖의 키워드 값은 그대로).
    fn keywords(
        &mut self,
        map: Map<String, Value>,
        depth: usize,
        at: &str,
    ) -> Result<Map<String, Value>, Problem> {
        let mut out = Map::new();
        for (key, value) in map {
            let value = match (key.as_str(), value) {
                ("properties", Value::Object(props)) => {
                    let mut next = Map::new();
                    for (name, sub) in props {
                        let here = format!("{at}/properties/{}", token(&name));
                        next.insert(name, self.schema(sub, depth + 1, false, &here)?);
                    }
                    Value::Object(next)
                }
                (k, sub @ (Value::Object(_) | Value::Bool(_))) if ONE.contains(&k) => {
                    self.schema(sub, depth + 1, false, &format!("{at}/{k}"))?
                }
                (k, Value::Array(list)) if MANY.contains(&k) => Value::Array(
                    list.into_iter()
                        .enumerate()
                        .map(|(i, sub)| {
                            self.schema(sub, depth + 1, false, &format!("{at}/{k}/{i}"))
                        })
                        .collect::<Result<_, _>>()?,
                ),
                (_, other) => other,
            };
            out.insert(key, value);
        }
        Ok(out)
    }

    /// `#/$defs/<이름>`·`#/definitions/<이름>`을 펼친 정의로. `at`은 참조를 가진 스키마의 자리다.
    /// 조각은 퍼센트 부호를 먼저 풀고(RFC 3986) 그 결과를 JSON 포인터로 읽는다(RFC 6901 §6): `~1`→`/`, `~0`→`~`.
    fn resolve(&mut self, reference: &str, depth: usize, at: &str) -> Result<Value, Problem> {
        let unsupported = |why: &str| {
            problem(
                "ref-unsupported",
                at,
                format!("지원하지 않는 $ref `{reference}` — {why}"),
            )
        };
        let Some(fragment) = reference.strip_prefix('#') else {
            return Err(problem(
                "ref-external",
                at,
                format!("외부 $ref는 지원하지 않아요: {reference}"),
            ));
        };
        if fragment.is_empty() {
            return Err(problem(
                "ref-recursive",
                at,
                "재귀 $ref(`#`: 문서 전체를 가리키는 참조)는 지원하지 않아요",
            ));
        }
        let pointer = percent_decode(fragment).ok_or_else(|| {
            unsupported("조각에 잘못된 퍼센트 표기(`%` 뒤 16진 두 자리)가 있어요")
        })?;
        let (space, token) = if let Some(rest) = pointer.strip_prefix("/$defs/") {
            ("$defs", rest)
        } else if let Some(rest) = pointer.strip_prefix("/definitions/") {
            ("definitions", rest)
        } else {
            return Err(unsupported(
                "`#/$defs/<이름>`·`#/definitions/<이름>`만 쓸 수 있어요",
            ));
        };
        if token.is_empty() || token.contains('/') {
            return Err(unsupported("정의 이름 하나만 가리킬 수 있어요"));
        }
        let name = unescape_token(token)
            .ok_or_else(|| unsupported("JSON 포인터의 `~`는 `~0`·`~1`로만 쓸 수 있어요"))?;
        let table = if space == "$defs" {
            &self.defs
        } else {
            &self.legacy
        };
        let Some(found) = table.get(&name).cloned() else {
            return Err(problem(
                "ref-missing",
                at,
                format!("$ref가 가리키는 정의가 없어요: {reference}"),
            ));
        };
        let key = format!("{space}/{name}");
        if let Some(from) = self.stack.iter().position(|k| *k == key) {
            let chain: Vec<&str> = self.stack[from..].iter().map(String::as_str).collect();
            return Err(problem(
                "ref-recursive",
                at,
                format!("재귀 $ref는 지원하지 않아요: {} → {key}", chain.join(" → ")),
            ));
        }
        if self.stack.len() >= MAX_CHAIN {
            return Err(problem(
                "ref-too-deep",
                at,
                format!("$ref 사슬이 너무 깊어요({MAX_CHAIN}단계 한도)"),
            ));
        }
        self.stack.push(key);
        let expanded = self.schema(found, depth + 1, false, at);
        self.stack.pop();
        expanded
    }
}

/// 퍼센트 부호(`%XX`)를 푼다. `%` 뒤에 16진 두 자리가 없거나 결과가 UTF-8이 아니면 `None`.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let (mut out, mut at) = (Vec::with_capacity(bytes.len()), 0);
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let digit = |offset: usize| (*bytes.get(at + offset)? as char).to_digit(16);
            out.push(u8::try_from(digit(1)? * 16 + digit(2)?).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// JSON 포인터 한 토막의 이스케이프를 푼다(`~1`→`/`, `~0`→`~`). `~` 뒤에 `0`·`1`이 아닌 것이 오거나 끝나면 `None`.
fn unescape_token(token: &str) -> Option<String> {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars();
    while let Some(c) = chars.next() {
        if c == '~' {
            out.push(match chars.next()? {
                '0' => '~',
                '1' => '/',
                _ => return None,
            });
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// 펼친 정의에 `$ref`와 같은 자리에 있던 키워드(`own`)를 얹는다. 주석성 키워드(`SIBLINGS`)만 받는다: 조건이 될 수 있는 형제(`allOf`·`type`·`minLength` 등, 모양이
/// 맞든 틀리든)는 정의의 조건을 바꾸거나 버릴 수 있으니 거절한다. 정의가 불리언 스키마이면 주석은 검증에 영향이 없어 정의 그대로다.
fn combine(target: Value, own: Map<String, Value>, at: &str) -> Result<Value, Problem> {
    if let Some(key) = own.keys().find(|key| !SIBLINGS.contains(&key.as_str())) {
        return Err(problem(
            "ref-sibling",
            at,
            format!(
                "`$ref` 옆에는 주석 키워드({})만 둘 수 있어요 — 조건 키워드 `{key}`는 정의 안으로 옮기세요",
                SIBLINGS.join("·")
            ),
        ));
    }
    Ok(match target {
        Value::Object(mut base) => {
            base.extend(own);
            Value::Object(base)
        }
        other => other,
    })
}

#[cfg(test)]
mod tests;
