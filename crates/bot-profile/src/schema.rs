//! Draft 2020-12 JSON Schema subset evaluator for bundled schemas and tool input schemas.
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
    /// JSON pointer to the offending value (empty for root).
    pub pointer: String,
    pub message: String,
}

/// Evaluation step budget preventing exponential blowup in nested combinators; fails closed once exceeded.
#[derive(Default)]
struct Cx {
    steps: Cell<usize>,
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
    pub fn compile_bundled(doc: Value) -> Result<Self, Vec<String>> {
        Self::build(doc, Dialect::Bundled)
    }

    pub fn compile_input(doc: Value) -> Result<Self, Vec<String>> {
        Self::compile_input_problems(doc).map_err(|found| found.iter().map(Problem::line).collect())
    }

    /// Compiles a tool input schema, returning structured codes and pointers (avoid exposing
    /// `Problem::message` for untrusted schemas because it may echo schema text).
    pub fn compile_input_problems(doc: Value) -> Result<Self, Vec<Problem>> {
        // Tool input schemas expand acyclic local `$ref` at compile time; bundled schemas resolve `$ref` lazily.
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
            push(
                &mut out,
                "",
                "schema is too complex (evaluation step limit exceeded)",
            );
        }
        // Deduplicate identical messages at the same pointer (e.g. overlapping pattern rules).
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
            Value::Bool(false) => return push(out, ptr, "value is not allowed"),
            Value::Object(o) => o,
            _ => return,
        };
        if depth > MAX_DEPTH {
            return push(out, ptr, "schema reference depth exceeded");
        }
        if let Some(Value::String(r)) = obj.get("$ref") {
            self.eval_ref(cx, r, inst, ptr, depth, out);
        }
        if let Some(t) = obj.get("type").filter(|t| !type_matches(t, inst)) {
            push(
                out,
                ptr,
                format!("expected {}, got {}", type_label(t), kind(inst)),
            );
        }
        if let Some(c) = obj.get("const").filter(|c| !json_equal(c, inst)) {
            push(out, ptr, format!("must equal {c}"));
        }
        if let Some(Value::Array(options)) = obj.get("enum")
            && !options.iter().any(|o| json_equal(o, inst))
        {
            let list: Vec<String> = options.iter().map(Value::to_string).collect();
            push(out, ptr, format!("must be one of: {}", list.join(", ")));
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
                    push(out, ptr, format!("invalid input schema: {e}"));
                }
            }
        } else if let Some(target) = r.strip_prefix('#').and_then(|p| self.doc.pointer(p)) {
            self.eval(cx, target, inst, ptr, depth + 1, out);
        } else {
            push(out, ptr, format!("unresolved reference: {r}"));
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
        // For closed objects, match unknown keys to unused known keys and suppress duplicate missing-required errors.
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
                push(out, ptr, format!("missing required field `{name}`"));
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
                                format!("unknown field `{key}`; did you mean `{meant}`?")
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
                        format!("key `{key}`: {}", v.message),
                    );
                }
            }
        }
        count_bounds(
            obj,
            ("minProperties", "maxProperties"),
            map.len(),
            ("field", "fields"),
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
        count_bounds(
            obj,
            ("minItems", "maxItems"),
            items.len(),
            ("item", "items"),
            ptr,
            out,
        );
        if obj.get("uniqueItems") == Some(&Value::Bool(true)) {
            let mut seen: Vec<&Value> = Vec::new();
            for item in items {
                if seen.iter().any(|prev| json_equal(prev, item)) {
                    push(out, ptr, format!("duplicate item: {item}"));
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
            ("character", "characters"),
            ptr,
            out,
        );
        if let Some(Value::String(p)) = obj.get("pattern")
            && !self.patterns.get(p).is_some_and(|re| re.is_match(s))
        {
            match hint::pattern_hint(p) {
                Some(text) => push(out, ptr, format!("invalid format: {text}")),
                None => push(out, ptr, format!("invalid format (pattern {p})")),
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
                _ => push(out, ptr, "matches more than one allowed schema"),
            }
        }
        if obj
            .get("not")
            .is_some_and(|sub| self.passes(cx, sub, inst, ptr, depth))
        {
            push(out, ptr, "matches a disallowed schema");
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

fn unknown_field(key: &str, props: Option<&Map<String, Value>>) -> String {
    match props {
        Some(known) if !known.is_empty() && known.len() <= 12 => {
            let names: Vec<&str> = known.keys().map(String::as_str).collect();
            format!(
                "unknown field `{key}`; allowed fields: {}",
                names.join(", ")
            )
        }
        _ => format!("unknown field `{key}`"),
    }
}

/// Returns a property fixed to a `const` across every union branch (e.g. `adapter`), if any.
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

/// Reports union failures using the discriminator branch when present, or the closest branch otherwise.
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
    let tail = meant.map_or(String::new(), |m| format!(" — did you mean \"{m}\"?"));
    push(
        out,
        &format!("{ptr}/{}", escape(key)),
        format!("must be one of {} (got {value}{tail})", list.join(", ")),
    );
}

fn closest(out: &mut Vec<Violation>, ptr: &str, runs: Vec<Vec<Violation>>) {
    match runs.into_iter().min_by_key(Vec::len) {
        Some(best) if !best.is_empty() => out.extend(best),
        _ => push(out, ptr, "does not match any allowed schema"),
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
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_label(t: &Value) -> String {
    let names: Vec<&str> = match t {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let label = |n: &str| match n {
        "null" => "null",
        "boolean" => "boolean",
        "object" => "object",
        "array" => "array",
        "string" => "string",
        "number" => "number",
        "integer" => "integer",
        _ => "?",
    };
    names
        .iter()
        .map(|n| label(n))
        .collect::<Vec<_>>()
        .join(" or ")
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
    unit: (&str, &str),
    ptr: &str,
    out: &mut Vec<Violation>,
) {
    if let Some(m) = obj
        .get(keys.0)
        .and_then(as_exact_u64)
        .filter(|m| (n as u64) < *m)
    {
        let label = if m == 1 { unit.0 } else { unit.1 };
        push(out, ptr, format!("must have at least {m} {label}"));
    }
    if let Some(m) = obj
        .get(keys.1)
        .and_then(as_exact_u64)
        .filter(|m| (n as u64) > *m)
    {
        let label = if m == 1 { unit.0 } else { unit.1 };
        push(out, ptr, format!("must have at most {m} {label}"));
    }
}

fn eval_number(obj: &Map<String, Value>, n: &Number, ptr: &str, out: &mut Vec<Violation>) {
    let get = |k: &str| obj.get(k).and_then(Value::as_number);
    if let Some(m) = get("minimum").filter(|m| num_cmp(n, m) == Some(Ordering::Less)) {
        push(out, ptr, format!("must be >= {m}"));
    }
    if let Some(m) = get("maximum").filter(|m| num_cmp(n, m) == Some(Ordering::Greater)) {
        push(out, ptr, format!("must be <= {m}"));
    }
    if let Some(m) = get("exclusiveMinimum")
        .filter(|m| matches!(num_cmp(n, m), Some(Ordering::Less | Ordering::Equal)))
    {
        push(out, ptr, format!("must be > {m}"));
    }
    if let Some(m) = get("exclusiveMaximum")
        .filter(|m| matches!(num_cmp(n, m), Some(Ordering::Greater | Ordering::Equal)))
    {
        push(out, ptr, format!("must be < {m}"));
    }
    if let Some(m) = get("multipleOf") {
        if let (Some(ni), Some(mi)) = (as_int(n), as_int(m)) {
            if mi > 0 && ni % mi != 0 {
                push(out, ptr, format!("must be a multiple of {m}"));
            }
        } else if let (Some(nf), Some(mf)) = (n.as_f64(), m.as_f64())
            && mf > 0.0
        {
            let q = nf / mf;
            if !q.is_finite() || (q - q.round()).abs() > 1e-9 {
                push(out, ptr, format!("must be a multiple of {m}"));
            }
        }
    }
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
