//! JSON Schema(2020-12)의 소집합 해석기. 번들한 스키마(schemas/v1)와 도구 `input` 스키마를 같은 코드로 실행한다.
//! 어떤 키워드를 받는지는 `vocab`이 정한다(지원하지 않는 키워드·외부 참조는 컴파일 단계에서 거절).
//! `pattern`은 선형 시간 정규식(regex)이라 악의적 패턴이 시간을 끌지 못한다.
mod value;

pub(crate) use value::{as_exact_i64, as_exact_u64};

use crate::{
    hint,
    vocab::{self, Dialect, MAX_DEPTH, META_REF, Problem},
};
use regex::Regex;
use serde_json::{Map, Number, Value};
use std::{
    cell::Cell,
    cmp::Ordering,
    collections::{HashMap, HashSet},
};
use value::{as_int, is_integer, json_equal, num_cmp};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Violation {
    /// 검사한 값의 JSON 포인터(`/bindings/0/adapter`, 최상위는 빈 문자열)
    pub pointer: String,
    pub message: String,
}

/// 한 번의 `validate`가 쓰는 검사 횟수. 중첩된 anyOf/oneOf가 지수로 불어나는 스키마를 끊는다.
#[derive(Default)]
struct Cx {
    steps: Cell<usize>,
    /// 한도를 넘으면 켠다. 이때 어느 가지도 믿을 수 없으므로 결과는 반드시 위반이다(닫힌 쪽으로 실패).
    over: Cell<bool>,
}

const MAX_STEPS: usize = 200_000;

#[derive(Debug)]
pub struct Schema {
    doc: Value,
    patterns: HashMap<String, Regex>,
    dialect: Dialect,
}

impl Schema {
    /// 번들한 스키마용. 메타 스키마 참조·`$ref`·`$defs` 등 전체 소집합을 허용한다.
    pub fn compile_bundled(doc: Value) -> Result<Self, Vec<String>> {
        Self::build(doc, Dialect::Bundled)
    }

    /// 도구 입력 스키마용. docs/plugins.md의 어휘만 허용한다.
    pub fn compile_input(doc: Value) -> Result<Self, Vec<String>> {
        Self::compile_input_problems(doc).map_err(|found| found.iter().map(Problem::line).collect())
    }

    /// `compile_input`과 같되 문제를 고정 코드와 JSON 포인터로 알린다(`Problem::message`는 스키마 안의 글을 되풀이할 수 있으니, 출처를 믿을 수 없는 스키마의 진단에는
    /// `code`와 `pointer`만 쓴다).
    pub fn compile_input_problems(doc: Value) -> Result<Self, Vec<Problem>> {
        // 도구 입력 스키마의 로컬 `$ref`(재귀 없는 것)는 컴파일 때 펼친다. 번들 스키마는 평가 때 `$ref`를 직접 따른다.
        let doc = crate::refs::expand(doc).map_err(|found| vec![found])?;
        let patterns = vocab::check_problems(&doc, Dialect::Input)?;
        Ok(Self {
            doc,
            patterns,
            dialect: Dialect::Input,
        })
    }

    fn build(doc: Value, dialect: Dialect) -> Result<Self, Vec<String>> {
        let patterns = vocab::check(&doc, dialect)?;
        Ok(Self {
            doc,
            patterns,
            dialect,
        })
    }

    pub fn doc(&self) -> &Value {
        &self.doc
    }

    pub fn validate(&self, instance: &Value) -> Vec<Violation> {
        let cx = Cx::default();
        let mut out = Vec::new();
        self.eval(&cx, &self.doc, instance, "", 0, &mut out);
        if cx.over.get() {
            push(&mut out, "", "스키마가 너무 복잡해요(검사 횟수 한도)");
        }
        // 같은 자리의 같은 문구는 한 번만 보인다(서로 다른 규칙이 같은 말을 하는 경우)
        let mut seen = HashSet::new();
        out.retain(|v| seen.insert((v.pointer.clone(), v.message.clone())));
        out
    }

    fn eval(
        &self,
        cx: &Cx,
        schema: &Value,
        inst: &Value,
        ptr: &str,
        depth: usize,
        out: &mut Vec<Violation>,
    ) {
        let steps = cx.steps.get() + 1;
        cx.steps.set(steps);
        if steps > MAX_STEPS {
            cx.over.set(true);
            return;
        }
        let obj = match schema {
            Value::Bool(true) => return,
            Value::Bool(false) => return push(out, ptr, "허용하지 않는 값이에요"),
            Value::Object(o) => o,
            _ => return,
        };
        if depth > MAX_DEPTH {
            return push(out, ptr, "스키마 참조가 너무 깊어요");
        }
        if let Some(Value::String(r)) = obj.get("$ref") {
            self.eval_ref(cx, r, inst, ptr, depth, out);
        }
        if let Some(t) = obj.get("type").filter(|t| !type_matches(t, inst)) {
            push(
                out,
                ptr,
                format!("{}이어야 해요(받은 값: {})", type_label(t), kind(inst)),
            );
        }
        if let Some(c) = obj.get("const").filter(|c| !json_equal(c, inst)) {
            push(out, ptr, format!("값은 {c}이어야 해요"));
        }
        if let Some(Value::Array(options)) = obj.get("enum")
            && !options.iter().any(|o| json_equal(o, inst))
        {
            let list: Vec<String> = options.iter().map(Value::to_string).collect();
            push(
                out,
                ptr,
                format!("허용 값이 아니에요(허용: {})", list.join(", ")),
            );
        }
        match inst {
            Value::Object(map) => self.eval_object(cx, obj, map, ptr, depth, out),
            Value::Array(items) => self.eval_array(cx, obj, items, ptr, depth, out),
            Value::String(s) => self.eval_string(obj, s, ptr, out),
            Value::Number(n) => eval_number(obj, n, ptr, out),
            _ => {}
        }
        self.eval_combinators(cx, obj, inst, ptr, depth, out);
    }

    fn eval_ref(
        &self,
        cx: &Cx,
        r: &str,
        inst: &Value,
        ptr: &str,
        depth: usize,
        out: &mut Vec<Violation>,
    ) {
        if r == META_REF && self.dialect == Dialect::Bundled {
            if let Err(errors) = Schema::compile_input(inst.clone()) {
                for e in errors {
                    push(out, ptr, format!("입력 스키마가 올바르지 않아요: {e}"));
                }
            }
        } else if let Some(target) = r.strip_prefix('#').and_then(|p| self.doc.pointer(p)) {
            self.eval(cx, target, inst, ptr, depth + 1, out);
        } else {
            push(out, ptr, format!("참조를 찾을 수 없어요: {r}"));
        }
    }

    fn eval_object(
        &self,
        cx: &Cx,
        obj: &Map<String, Value>,
        map: &Map<String, Value>,
        ptr: &str,
        depth: usize,
        out: &mut Vec<Violation>,
    ) {
        let props = obj.get("properties").and_then(Value::as_object);
        // 오타: 알 수 없는 필드마다 비슷한 알려진 필드를 찾는다(이미 쓴 필드는 제안하지 않는다). 필수 필드의 오타면 «없어요» 줄은 따로 내지 않는다.
        let closed = obj.get("additionalProperties") == Some(&Value::Bool(false));
        let typos: Vec<(&str, &str)> = match props.filter(|_| closed) {
            Some(known) => map
                .keys()
                .filter(|key| !known.contains_key(*key))
                .filter_map(|key| {
                    let free = known
                        .keys()
                        .map(String::as_str)
                        .filter(|k| !map.contains_key(*k));
                    hint::nearest(key, free).map(|meant| (key.as_str(), meant))
                })
                .collect(),
            None => Vec::new(),
        };
        if let Some(Value::Array(required)) = obj.get("required") {
            for name in required
                .iter()
                .filter_map(Value::as_str)
                .filter(|n| !map.contains_key(*n))
                .filter(|n| !typos.iter().any(|(_, meant)| meant == n))
            {
                push(out, ptr, format!("필수 필드 `{name}`이 없어요"));
            }
        }
        for (key, sub) in props.into_iter().flatten() {
            if let Some(value) = map.get(key) {
                self.eval(
                    cx,
                    sub,
                    value,
                    &format!("{ptr}/{}", escape(key)),
                    depth + 1,
                    out,
                );
            }
        }
        if let Some(additional) = obj.get("additionalProperties") {
            for (key, value) in map
                .iter()
                .filter(|(k, _)| !props.is_some_and(|p| p.contains_key(*k)))
            {
                let at = format!("{ptr}/{}", escape(key));
                match additional {
                    Value::Bool(false) => {
                        let message = match typos.iter().find(|(typo, _)| typo == key) {
                            Some((_, meant)) => {
                                format!(
                                    "알 수 없는 필드 `{key}`예요. `{meant}`를 잘못 쓴 것 같아요"
                                )
                            }
                            None => unknown_field(key, props),
                        };
                        push(out, &at, message);
                    }
                    sub => self.eval(cx, sub, value, &at, depth + 1, out),
                }
            }
        }
        if let Some(names) = obj.get("propertyNames") {
            for key in map.keys() {
                let mut found = Vec::new();
                self.eval(
                    cx,
                    names,
                    &Value::String(key.clone()),
                    "",
                    depth + 1,
                    &mut found,
                );
                for v in found {
                    push(
                        out,
                        &format!("{ptr}/{}", escape(key)),
                        format!("키 `{key}`: {}", v.message),
                    );
                }
            }
        }
        count_bounds(
            obj,
            ("minProperties", "maxProperties"),
            map.len(),
            "필드",
            ptr,
            out,
        );
    }

    fn eval_array(
        &self,
        cx: &Cx,
        obj: &Map<String, Value>,
        items: &[Value],
        ptr: &str,
        depth: usize,
        out: &mut Vec<Violation>,
    ) {
        if let Some(sub) = obj.get("items") {
            for (i, item) in items.iter().enumerate() {
                self.eval(cx, sub, item, &format!("{ptr}/{i}"), depth + 1, out);
            }
        }
        count_bounds(obj, ("minItems", "maxItems"), items.len(), "항목", ptr, out);
        if obj.get("uniqueItems") == Some(&Value::Bool(true)) {
            let mut seen: Vec<&Value> = Vec::new();
            for item in items {
                if seen.iter().any(|prev| json_equal(prev, item)) {
                    push(out, ptr, format!("중복된 항목이 있어요: {item}"));
                } else {
                    seen.push(item);
                }
            }
        }
    }

    fn eval_string(&self, obj: &Map<String, Value>, s: &str, ptr: &str, out: &mut Vec<Violation>) {
        count_bounds(
            obj,
            ("minLength", "maxLength"),
            s.chars().count(),
            "글자",
            ptr,
            out,
        );
        if let Some(Value::String(p)) = obj.get("pattern")
            && !self.patterns.get(p).is_some_and(|re| re.is_match(s))
        {
            // 번들 스키마의 정규식은 사람 말로, 도구가 정한 패턴은 원문 그대로 보인다
            match hint::pattern_hint(p) {
                Some(text) => push(out, ptr, format!("형식이 맞지 않아요. {text}")),
                None => push(out, ptr, format!("형식이 맞지 않아요(패턴 {p})")),
            }
        }
    }

    fn eval_combinators(
        &self,
        cx: &Cx,
        obj: &Map<String, Value>,
        inst: &Value,
        ptr: &str,
        depth: usize,
        out: &mut Vec<Violation>,
    ) {
        if let Some(Value::Array(all)) = obj.get("allOf") {
            for sub in all {
                self.eval(cx, sub, inst, ptr, depth + 1, out);
            }
        }
        if let Some(Value::Array(any)) = obj.get("anyOf") {
            let runs = self.branches(cx, any, inst, ptr, depth);
            if !runs.iter().any(Vec::is_empty) {
                report_union(out, ptr, any, inst, runs);
            }
        }
        if let Some(Value::Array(one)) = obj.get("oneOf") {
            let runs = self.branches(cx, one, inst, ptr, depth);
            match runs.iter().filter(|r| r.is_empty()).count() {
                1 => {}
                0 => report_union(out, ptr, one, inst, runs),
                _ => push(out, ptr, "허용하는 형식 둘 이상과 동시에 맞아요"),
            }
        }
        if obj
            .get("not")
            .is_some_and(|sub| self.passes(cx, sub, inst, ptr, depth))
        {
            push(out, ptr, "허용하지 않는 형태예요");
        }
        if let Some(cond) = obj.get("if") {
            let branch = if self.passes(cx, cond, inst, ptr, depth) {
                "then"
            } else {
                "else"
            };
            if let Some(sub) = obj.get(branch) {
                self.eval(cx, sub, inst, ptr, depth + 1, out);
            }
        }
    }

    fn branches(
        &self,
        cx: &Cx,
        subs: &[Value],
        inst: &Value,
        ptr: &str,
        depth: usize,
    ) -> Vec<Vec<Violation>> {
        subs.iter()
            .map(|sub| {
                let mut found = Vec::new();
                self.eval(cx, sub, inst, ptr, depth + 1, &mut found);
                found
            })
            .collect()
    }

    fn passes(&self, cx: &Cx, sub: &Value, inst: &Value, ptr: &str, depth: usize) -> bool {
        let mut found = Vec::new();
        self.eval(cx, sub, inst, ptr, depth + 1, &mut found);
        found.is_empty()
    }
}

/// 알 수 없는 필드의 문구: 쓸 수 있는 필드가 많지 않으면 그 목록을 함께 보인다.
fn unknown_field(key: &str, props: Option<&Map<String, Value>>) -> String {
    match props {
        Some(known) if !known.is_empty() && known.len() <= 12 => {
            let names: Vec<&str> = known.keys().map(String::as_str).collect();
            format!(
                "알 수 없는 필드 `{key}`예요. 쓸 수 있는 필드: {}",
                names.join(", ")
            )
        }
        _ => format!("알 수 없는 필드 `{key}`"),
    }
}

/// 모든 가지가 같은 필드를 서로 다른 `const`로 고정해 두었으면(예: `adapter`) 그 필드의 이름과 가지별 값. 판별 필드가 없으면 `None`.
fn discriminator(subs: &[Value]) -> Option<(&str, Vec<&Value>)> {
    let first = subs.first()?.get("properties")?.as_object()?;
    first.keys().find_map(|key| {
        let consts: Option<Vec<&Value>> = subs
            .iter()
            .map(|sub| sub.get("properties")?.get(key)?.get("const"))
            .collect();
        Some((key.as_str(), consts?))
    })
}

/// 어느 가지도 안 맞을 때의 보고. 판별 필드가 있으면 그 값으로 가지를 고른다: 값이 어느 가지의 것도 아니면 허용 값을 모두 보이고(비슷한 값을 제안),
/// 어느 가지의 것이면 그 가지들 중에서 가장 가까운 가지의 위반만 보인다. 판별 필드가 없거나 인스턴스에 없으면 전체에서 가장 가까운 가지다.
fn report_union(
    out: &mut Vec<Violation>,
    ptr: &str,
    subs: &[Value],
    inst: &Value,
    runs: Vec<Vec<Violation>>,
) {
    let found = discriminator(subs).and_then(|(key, consts)| {
        let value = inst.as_object()?.get(key)?;
        Some((key, consts, value))
    });
    let Some((key, consts, value)) = found else {
        return closest(out, ptr, runs);
    };
    if consts.iter().any(|tag| json_equal(tag, value)) {
        let matching = runs
            .into_iter()
            .zip(&consts)
            .filter(|(_, tag)| json_equal(tag, value))
            .map(|(run, _)| run)
            .collect();
        return closest(out, ptr, matching);
    }
    let mut allowed: Vec<&Value> = Vec::new();
    for tag in &consts {
        if !allowed.iter().any(|seen| json_equal(seen, tag)) {
            allowed.push(tag);
        }
    }
    let list: Vec<String> = allowed.iter().map(|tag| tag.to_string()).collect();
    let meant = value
        .as_str()
        .and_then(|text| hint::nearest(text, allowed.iter().filter_map(|tag| tag.as_str())));
    let tail = meant.map_or(String::new(), |m| format!(" — 혹시 \"{m}\"인가요?"));
    push(
        out,
        &format!("{ptr}/{}", escape(key)),
        format!(
            "허용 값은 {} 중 하나예요(받은 값: {value}{tail})",
            list.join(", ")
        ),
    );
}

/// 어느 가지도 안 맞을 때, 위반이 가장 적은(가장 가까운) 가지의 위반을 보여 준다.
fn closest(out: &mut Vec<Violation>, ptr: &str, runs: Vec<Vec<Violation>>) {
    match runs.into_iter().min_by_key(Vec::len) {
        Some(best) if !best.is_empty() => out.extend(best),
        _ => push(out, ptr, "허용하는 형식 중 어느 것과도 맞지 않아요"),
    }
}

fn push(out: &mut Vec<Violation>, ptr: &str, message: impl Into<String>) {
    out.push(Violation {
        pointer: ptr.to_owned(),
        message: message.into(),
    });
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "불리언",
        Value::Number(_) => "숫자",
        Value::String(_) => "문자열",
        Value::Array(_) => "배열",
        Value::Object(_) => "객체",
    }
}

fn type_label(t: &Value) -> String {
    let names: Vec<&str> = match t {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let ko = |n: &str| match n {
        "null" => "null",
        "boolean" => "불리언",
        "object" => "객체",
        "array" => "배열",
        "string" => "문자열",
        "number" => "숫자",
        "integer" => "정수",
        _ => "?",
    };
    names
        .iter()
        .map(|n| ko(n))
        .collect::<Vec<_>>()
        .join(" 또는 ")
}

fn type_matches(t: &Value, inst: &Value) -> bool {
    let one = |name: &str| match (name, inst) {
        ("null", Value::Null)
        | ("boolean", Value::Bool(_))
        | ("object", Value::Object(_))
        | ("array", Value::Array(_))
        | ("string", Value::String(_))
        | ("number", Value::Number(_)) => true,
        ("integer", Value::Number(n)) => is_integer(n),
        _ => false,
    };
    match t {
        Value::String(s) => one(s),
        Value::Array(a) => a.iter().filter_map(Value::as_str).any(one),
        _ => true,
    }
}

fn count_bounds(
    obj: &Map<String, Value>,
    keys: (&str, &str),
    n: usize,
    unit: &str,
    ptr: &str,
    out: &mut Vec<Violation>,
) {
    if let Some(m) = obj
        .get(keys.0)
        .and_then(as_exact_u64)
        .filter(|m| (n as u64) < *m)
    {
        push(out, ptr, format!("{unit}이(가) {m}개 이상이어야 해요"));
    }
    if let Some(m) = obj
        .get(keys.1)
        .and_then(as_exact_u64)
        .filter(|m| (n as u64) > *m)
    {
        push(out, ptr, format!("{unit}이(가) {m}개 이하여야 해요"));
    }
}

fn eval_number(obj: &Map<String, Value>, n: &Number, ptr: &str, out: &mut Vec<Violation>) {
    let get = |k: &str| obj.get(k).and_then(Value::as_number);
    if let Some(m) = get("minimum").filter(|m| num_cmp(n, m) == Some(Ordering::Less)) {
        push(out, ptr, format!("{m} 이상이어야 해요"));
    }
    if let Some(m) = get("maximum").filter(|m| num_cmp(n, m) == Some(Ordering::Greater)) {
        push(out, ptr, format!("{m} 이하여야 해요"));
    }
    if let Some(m) = get("exclusiveMinimum")
        .filter(|m| matches!(num_cmp(n, m), Some(Ordering::Less | Ordering::Equal)))
    {
        push(out, ptr, format!("{m}보다 커야 해요"));
    }
    if let Some(m) = get("exclusiveMaximum")
        .filter(|m| matches!(num_cmp(n, m), Some(Ordering::Greater | Ordering::Equal)))
    {
        push(out, ptr, format!("{m}보다 작아야 해요"));
    }
    if let Some(m) = get("multipleOf") {
        if let (Some(ni), Some(mi)) = (as_int(n), as_int(m)) {
            if mi > 0 && ni % mi != 0 {
                push(out, ptr, format!("{m}의 배수여야 해요"));
            }
        } else if let (Some(nf), Some(mf)) = (n.as_f64(), m.as_f64())
            && mf > 0.0
        {
            let q = nf / mf;
            if !q.is_finite() || (q - q.round()).abs() > 1e-9 {
                push(out, ptr, format!("{m}의 배수여야 해요"));
            }
        }
    }
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
