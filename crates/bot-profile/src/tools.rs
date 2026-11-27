//! Parsing and local validation of `tools.yaml` declarations and `tools.lock.yaml` linkage.
use crate::{
    hash::{canonical_json, sha256_hex},
    issue::Issues,
    limits::Limits,
    lock::Lock,
    model::Tool,
    schema::Schema,
};
use bot_core::{
    command::JsonPayload,
    id::{Revision, SecretName, ToolId},
    profile::Destination,
    state::Risk,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

fn strings(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse(
    value: &Value,
    skill: &str,
    path: &str,
    limits: &Limits,
    lock: Option<&Lock>,
    issues: &mut Issues,
) -> Option<Vec<Arc<Tool>>> {
    let root = value.as_object()?;
    let server = root.get("server").and_then(Value::as_str)?.to_owned();
    let list = root.get("tools").and_then(Value::as_array)?;
    if let Some(lock) = lock.filter(|lock| lock.server != server) {
        issues.error_at(
            "lock-server-mismatch",
            &lock.path,
            "/server",
            format!(
                "tools.yaml targets server `{server}`, but lock file is for `{}`",
                lock.server
            ),
        );
        return None;
    }
    if list.len() > limits.max_tools {
        issues.error_at(
            "too-many",
            path,
            "/tools",
            format!("too many tools (max {})", limits.max_tools),
        );
        return None;
    }
    let before = issues.error_count();
    let mut tools: Vec<Arc<Tool>> = Vec::new();
    for (i, item) in list.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            continue;
        };
        if let Some(tool) = parse_tool(
            obj,
            skill,
            &server,
            path,
            &format!("/tools/{i}"),
            lock,
            issues,
        ) {
            if tools.iter().any(|t| t.id == tool.id) {
                issues.error_at(
                    "duplicate-tool",
                    path,
                    format!("/tools/{i}/id"),
                    format!("duplicate tool id: {}", tool.name),
                );
            } else {
                tools.push(Arc::new(tool));
            }
        }
    }
    if let Some(lock) = lock {
        let declared: BTreeSet<&str> = list
            .iter()
            .filter_map(|item| item.get("id").and_then(Value::as_str))
            .collect();
        for id in lock
            .tools
            .keys()
            .filter(|id| !declared.contains(id.as_str()))
        {
            issues.warning(
                "lock-unused",
                &lock.path,
                format!(
                    "`{id}` is not declared in tools.yaml and will be ignored — re-run `bot mcp sync` to clean it up"
                ),
            );
        }
    }
    (issues.error_count() == before).then_some(tools)
}

fn parse_tool(
    obj: &Map<String, Value>,
    skill: &str,
    server: &str,
    path: &str,
    at: &str,
    lock: Option<&Lock>,
    issues: &mut Issues,
) -> Option<Tool> {
    let name = obj.get("id").and_then(Value::as_str)?.to_owned();
    if bot_core::builtin::reserved(&format!("{skill}.{name}")) {
        issues.error_at(
            "reserved-tool",
            path,
            format!("{at}/id"),
            format!("`{skill}.{name}` is a reserved host built-in tool name"),
        );
        return None;
    }
    let class = match obj.get("class").and_then(Value::as_str)? {
        "read" => Risk::Read,
        "write" => Risk::Write,
        _ => Risk::Irreversible,
    };
    // `name` overrides the remote MCP tool name; omitting or matching `id` leaves `mcp_name` as `None` (preserving revision).
    let mcp_name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|given| *given != name)
        .map(str::to_owned);
    let remote_name = mcp_name.as_deref().unwrap_or(&name);
    // Only tools without inline `input` inherit schema, description, and definition hash from `tools.lock.yaml`.
    let (input, locked) = match (obj.get("input"), lock.and_then(|l| l.tools.get(&name))) {
        (Some(inline), _) => (inline.clone(), None),
        (None, Some(entry)) if entry.name != remote_name => {
            issues.error_at(
                "lock-name-mismatch",
                path,
                format!("{at}/name"),
                format!(
                    "tools.yaml maps this tool to server name `{remote_name}`, but the lock records `{}` — re-run `bot mcp sync <skill-dir>`",
                    entry.name
                ),
            );
            return None;
        }
        (None, Some(entry)) => (entry.input_schema.clone(), Some(entry)),
        (None, None) => {
            issues.error_at(
                "missing-input",
                path,
                format!("{at}/input"),
                "omitting `input` requires this tool to be present in tools.lock.yaml — run `bot mcp sync <skill-dir>` or specify `input` inline",
            );
            return None;
        }
    };
    let definition = locked.map(|entry| entry.sha256.clone());
    let before = issues.error_count();
    let schema = compile_input(&input, path, at, issues);
    // Validate destination argument paths against the `$ref`-expanded schema.
    let expanded = schema.as_ref().map_or(&input, |compiled| compiled.doc());
    let destination = parse_destination(obj.get("destination"), class, expanded, path, at, issues);
    let egress = parse_egress(obj.get("egress"), path, at, issues);
    let secrets: BTreeMap<String, SecretName> = obj
        .get("secrets")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(env, v)| Some((env.clone(), SecretName::parse(v.as_str()?)?)))
                .collect()
        })
        .unwrap_or_default();
    if issues.error_count() != before {
        return None;
    }
    let flag = |key: &str| obj.get(key).and_then(Value::as_bool).unwrap_or(false);
    // Optional per-call timeout (1..=900s); `None` defaults to 60s and omits the key from revision hashing.
    let timeout_secs = obj
        .get("timeout_secs")
        .and_then(crate::schema::as_exact_u64)
        .and_then(|secs| u32::try_from(secs).ok());
    let scopes = strings(obj.get("scopes"));
    let env = strings(obj.get("env"));
    let mut declared = json!({
        "server": server,
        "id": format!("{skill}.{name}"),
        "class": obj.get("class"),
        "scopes": scopes,
        "external": flag("external"),
        "effects": flag("effects"),
        "destination": obj.get("destination"),
        "secrets": obj.get("secrets"),
        "egress": egress,
        "input": input,
    });
    if !env.is_empty() {
        declared["env"] = json!(env);
    }
    // Hash optional definition digest, remote name, and explicit timeout into revision to invalidate stale approvals.
    if let Some(sha) = &definition {
        declared["definition"] = json!(sha);
    }
    if let Some(given) = &mcp_name {
        declared["name"] = json!(given);
    }
    if let Some(secs) = timeout_secs {
        declared["timeout_secs"] = json!(secs);
    }
    Some(Tool {
        id: ToolId(format!("{skill}.{name}")),
        name,
        server: server.to_owned(),
        class,
        scopes,
        external: flag("external"),
        effects: flag("effects"),
        destination,
        env,
        secrets,
        egress,
        input: JsonPayload(canonical_json(&input)),
        description: locked
            .filter(|entry| !entry.description.is_empty())
            .map(|entry| entry.description.clone()),
        revision: Revision(sha256_hex(canonical_json(&declared).as_bytes())),
        schema: Arc::new(schema?),
        definition,
        mcp_name,
        timeout_secs,
    })
}

fn compile_input(input: &Value, path: &str, at: &str, issues: &mut Issues) -> Option<Schema> {
    let at = format!("{at}/input");
    match Schema::compile_input(input.clone()) {
        Ok(schema) if schema.doc().get("type") != Some(&json!("object")) => {
            issues.error_at(
                "bad-input-schema",
                path,
                at,
                "input schema top-level must be `type: object`",
            );
            None
        }
        Ok(schema) => Some(schema),
        Err(errors) => {
            for e in errors {
                issues.error_at("bad-input-schema", path, at.clone(), e);
            }
            None
        }
    }
}

fn parse_destination(
    value: Option<&Value>,
    class: Risk,
    input: &Value,
    path: &str,
    at: &str,
    issues: &mut Issues,
) -> Option<Destination> {
    let value = value?;
    let at = format!("{at}/destination");
    if class == Risk::Read {
        issues.error_at(
            "destination-on-read",
            path,
            at,
            "read tools cannot declare a destination (write tools only)",
        );
        return None;
    }
    let Some(obj) = value.as_object() else {
        return Some(Destination::Reply);
    };
    let paths = ["recipient", "content"].map(|key| {
        let p = obj.get(key).and_then(Value::as_str).unwrap_or_default();
        let first = p
            .strip_prefix("args.")
            .and_then(|r| r.split('.').next())
            .unwrap_or_default();
        let known = input
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|props| props.contains_key(first));
        if !known {
            issues.error_at(
                "bad-destination",
                path,
                format!("{at}/{key}"),
                format!("argument `{first}` referenced by `{p}` is not in input.properties"),
            );
        }
        p.to_owned()
    });
    let [recipient, content] = paths;
    Some(Destination::Explicit { recipient, content })
}

/// Validates `host:port` egress entries (lowercase DNS labels <= 63 chars, port 1..=65535).
fn parse_egress(
    value: Option<&Value>,
    path: &str,
    at: &str,
    issues: &mut Issues,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for entry in value
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let Some(text) = entry.as_str() else { continue };
        let ok = text.rsplit_once(':').is_some_and(|(host, port)| {
            port.parse::<u16>().is_ok_and(|p| p > 0)
                && host.len() <= 253
                && host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                })
        });
        if ok {
            out.insert(text.to_owned());
        } else {
            issues.error_at(
                "bad-egress",
                path,
                format!("{at}/egress"),
                format!("invalid egress entry (expected `host:port` with port 1..=65535): {text}"),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests;
