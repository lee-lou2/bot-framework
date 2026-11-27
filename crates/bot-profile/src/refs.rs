//! Acyclic local `$ref` expansion for tool input schemas (`#/$defs/*` and `#/definitions/*`).
use crate::vocab::Problem;
use serde_json::{Map, Value};

const MAX_NODES: usize = 20_000;
const MAX_DEPTH: usize = 64;
const MAX_CHAIN: usize = 32;

/// Annotation keywords allowed alongside `$ref` (constraint siblings are rejected so conditions are never silently weakened).
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

const ONE: &[&str] = &[
    "items",
    "additionalProperties",
    "propertyNames",
    "not",
    "if",
    "then",
    "else",
];

const MANY: &[&str] = &["allOf", "anyOf", "oneOf"];

struct Expander {
    defs: Map<String, Value>,
    legacy: Map<String, Value>,
    nodes: usize,
    /// Active definition stack used to detect recursive cycles.
    stack: Vec<String>,
}

fn problem(code: &'static str, pointer: &str, message: impl Into<String>) -> Problem {
    Problem {
        code,
        pointer: pointer.to_owned(),
        message: message.into(),
    }
}

fn token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Expands acyclic local `$ref` definitions in place and removes root `$defs`/`definitions`.
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
            format!("`{key}` must be an object"),
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
                    "expanded `$ref` schema is too large (limit: {MAX_NODES} schema nodes) — check for exponential shared references"
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
            return Err(problem("too-deep", at, "schema is too deeply nested"));
        }
        self.count(at)?;
        if !root && (map.contains_key("$defs") || map.contains_key("definitions")) {
            return Err(problem(
                "defs-nested",
                at,
                "`$defs` and `definitions` are only allowed at the schema root",
            ));
        }
        let reference = match map.remove("$ref") {
            None => None,
            Some(Value::String(text)) => Some(text),
            Some(_) => return Err(problem("bad-ref", at, "$ref must be a string")),
        };
        let own = self.keywords(map, depth, at)?;
        let Some(reference) = reference else {
            return Ok(Value::Object(own));
        };
        let target = self.resolve(&reference, depth, at)?;
        combine(target, own, at)
    }

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

    /// Resolves `#/$defs/<name>` or `#/definitions/<name>`; percent-decodes (RFC 3986) before JSON pointer unescaping (RFC 6901 §6).
    fn resolve(&mut self, reference: &str, depth: usize, at: &str) -> Result<Value, Problem> {
        let unsupported = |why: &str| {
            problem(
                "ref-unsupported",
                at,
                format!("unsupported $ref `{reference}` — {why}"),
            )
        };
        let Some(fragment) = reference.strip_prefix('#') else {
            return Err(problem(
                "ref-external",
                at,
                format!("external $ref is not supported: {reference}"),
            ));
        };
        if fragment.is_empty() {
            return Err(problem(
                "ref-recursive",
                at,
                "recursive $ref (`#` referencing the whole document) is not supported",
            ));
        }
        let pointer = percent_decode(fragment)
            .ok_or_else(|| unsupported("invalid percent-encoding in fragment"))?;
        let (space, token) = if let Some(rest) = pointer.strip_prefix("/$defs/") {
            ("$defs", rest)
        } else if let Some(rest) = pointer.strip_prefix("/definitions/") {
            ("definitions", rest)
        } else {
            return Err(unsupported(
                "only `#/$defs/<name>` and `#/definitions/<name>` are supported",
            ));
        };
        if token.is_empty() || token.contains('/') {
            return Err(unsupported("must reference a single definition name"));
        }
        let name = unescape_token(token)
            .ok_or_else(|| unsupported("invalid JSON pointer escape (`~` must be `~0` or `~1`)"))?;
        let table = if space == "$defs" {
            &self.defs
        } else {
            &self.legacy
        };
        let Some(found) = table.get(&name).cloned() else {
            return Err(problem(
                "ref-missing",
                at,
                format!("$ref target definition not found: {reference}"),
            ));
        };
        let key = format!("{space}/{name}");
        if let Some(from) = self.stack.iter().position(|k| *k == key) {
            let chain: Vec<&str> = self.stack[from..].iter().map(String::as_str).collect();
            return Err(problem(
                "ref-recursive",
                at,
                format!(
                    "recursive $ref is not supported: {} → {key}",
                    chain.join(" → ")
                ),
            ));
        }
        if self.stack.len() >= MAX_CHAIN {
            return Err(problem(
                "ref-too-deep",
                at,
                format!("$ref chain is too deep (limit: {MAX_CHAIN})"),
            ));
        }
        self.stack.push(key);
        let expanded = self.schema(found, depth + 1, false, at);
        self.stack.pop();
        expanded
    }
}

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

/// Overlays annotation siblings onto the resolved target; rejects non-annotation siblings.
fn combine(target: Value, own: Map<String, Value>, at: &str) -> Result<Value, Problem> {
    if let Some(key) = own.keys().find(|key| !SIBLINGS.contains(&key.as_str())) {
        return Err(problem(
            "ref-sibling",
            at,
            format!(
                "only annotation keywords ({}) may appear alongside `$ref` — move constraint `{key}` into the definition",
                SIBLINGS.join(", ")
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
