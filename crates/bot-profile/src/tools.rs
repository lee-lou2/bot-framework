//! tools.yaml 해석: 도구 선언을 `Tool`로 만들고, 파일 안에서 정해지는 것만 검사한다.
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

/// `lock`은 같은 스킬 폴더의 `tools.lock.yaml`(검증을 마친 것). 도구가 `input`을 생략하면 입력 스키마와 정의 해시를 여기서 가져온다.
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
                "tools.yaml의 서버는 `{server}`인데 이 잠금은 `{}`의 것이에요",
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
            format!("도구가 너무 많아요(최대 {})", limits.max_tools),
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
                    format!("도구 id가 겹쳐요: {}", tool.name),
                );
            } else {
                tools.push(Arc::new(tool));
            }
        }
    }
    if let Some(lock) = lock {
        // 선언에 없는 항목은 쓰이지 않는다(`bot mcp sync`는 선언한 도구만 적으므로 도구를 지운 뒤 다시 동기화하면 사라진다)
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
                format!("`{id}`는 tools.yaml에 선언돼 있지 않아 쓰이지 않아요 — `bot mcp sync`로 다시 만들면 사라져요"),
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
            format!("`{skill}.{name}`는 호스트 내장 도구 이름이라 선언할 수 없어요"),
        );
        return None;
    }
    let class = match obj.get("class").and_then(Value::as_str)? {
        "read" => Risk::Read,
        "write" => Risk::Write,
        _ => Risk::Irreversible,
    };
    // 서버가 내세우는 도구 이름: `name`(MCP 이름 규칙, 번들 스키마가 모양을 본다), 생략하면 id. id와 같으면 쓰지 않은 것과 같다(판본도 그대로).
    let mcp_name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|given| *given != name)
        .map(str::to_owned);
    let remote_name = mcp_name.as_deref().unwrap_or(&name);
    // `input`을 적으면 그것이 정본이다(실행할 때 서버의 입력 스키마가 같은지만 본다). 생략하면 잠금 파일의 스키마와 정의 해시가 정본이다.
    let (input, definition) = match (obj.get("input"), lock.and_then(|l| l.tools.get(&name))) {
        // (`bot mcp sync`는 선언한 도구를 모두 적으므로 인라인 도구의 항목은 있어도 쓰이지 않는다 — 경고하지 않는다)
        (Some(inline), _) => (inline.clone(), None),
        (None, Some(entry)) if entry.name != remote_name => {
            issues.error_at(
                "lock-name-mismatch",
                path,
                format!("{at}/name"),
                format!(
                    "tools.yaml는 이 도구의 서버 이름을 `{remote_name}`로 정했는데 잠금은 `{}`로 적었어요 — `bot mcp sync <스킬 폴더>`로 다시 만드세요",
                    entry.name
                ),
            );
            return None;
        }
        (None, Some(entry)) => (entry.input_schema.clone(), Some(entry.sha256.clone())),
        (None, None) => {
            issues.error_at(
                "missing-input",
                path,
                format!("{at}/input"),
                "input을 생략하려면 같은 폴더의 tools.lock.yaml에 이 도구가 있어야 해요 — `bot mcp sync <스킬 폴더>`로 만들거나 input을 적으세요",
            );
            return None;
        }
    };
    let before = issues.error_count();
    let schema = compile_input(&input, path, at, issues);
    // 목적지 경로는 `$ref`를 펼친 스키마의 속성으로 확인한다(원본 스키마의 최상위가 `$ref`일 수 있다)
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
    // 호출 시간 한도(초): 스키마가 1~900의 정수만 받는다. 선언하지 않으면 `None`(기본 60초)이고 도구 판본도 그대로다.
    let timeout_secs = obj
        .get("timeout_secs")
        .and_then(Value::as_u64)
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
    // 정의 해시는 판본에 든다: 서버의 설명·스키마가 바뀌어 잠금을 다시 만들면 도구 판본이 달라져 옛 승인이 새 정의에 쓰이지 않는다
    if let Some(sha) = &definition {
        declared["definition"] = json!(sha);
    }
    // 다른 서버 이름을 가리키게 바꾸면 다른 도구다(id와 같은 이름은 쓰지 않은 것과 같아 옛 판본이 그대로다)
    if let Some(given) = &mcp_name {
        declared["name"] = json!(given);
    }
    // 호출 시간 한도를 선언하면 판본에 든다: 한도를 바꾸면 도구 판본이 달라져 옛 승인이 새 한도에 쓰이지 않는다(선언하지 않은 도구의 판본은 그대로다)
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
        revision: Revision(sha256_hex(canonical_json(&declared).as_bytes())),
        schema: Arc::new(schema?),
        definition,
        mcp_name,
        timeout_secs,
    })
}

/// 인자는 항상 객체이므로 입력 스키마의 최상위는 `type: object`여야 하고, 지원하는 키워드만 써야 한다(로컬 `$ref`는 재귀 없는 것만 펼쳐 쓴다).
fn compile_input(input: &Value, path: &str, at: &str, issues: &mut Issues) -> Option<Schema> {
    let at = format!("{at}/input");
    match Schema::compile_input(input.clone()) {
        Ok(schema) if schema.doc().get("type") != Some(&json!("object")) => {
            issues.error_at(
                "bad-input-schema",
                path,
                at,
                "input의 최상위는 `type: object`여야 해요",
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
            "read 도구는 전송 목적지를 가질 수 없어요(쓰기 도구만 선언)",
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
                format!("`{p}`가 가리키는 인자 `{first}`가 input.properties에 없어요"),
            );
        }
        p.to_owned()
    });
    let [recipient, content] = paths;
    Some(Destination::Explicit { recipient, content })
}

/// `host:port` — 호스트는 소문자 라벨(63자 이하, 양끝 `-` 금지), 포트는 1~65535
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
                format!("허용 호스트 형식이 아니에요(`host:port`, 포트 1~65535): {text}"),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests;
