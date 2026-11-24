//! tools.yaml 해석: 도구 선언을 `Tool`로 만들고, 파일 안에서 정해지는 것만 검사한다.
use crate::{
    hash::{canonical_json, sha256_hex},
    issue::Issues,
    limits::Limits,
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

/// 호스트가 내장 도구로 쓰는 전체 도구 ID(기억 공유, 대화 중 예약·후속 확인, 호스트 자동 회신 승인 전용 `schedule.deliver`). 정확히 이 ID만 예약하고,
/// 같은 이름의 스킬(`memory`, `schedule`)이나 같은 도구 이름(`mail.share`, `calendar.create`)은 막지 않는다.
/// 점 없는 내장 이름(`delegate`)은 `skill.tool`과 겹칠 수 없다.
const RESERVED_TOOL_IDS: &[&str] = &[
    "memory.share",
    "schedule.create",
    "schedule.list",
    "schedule.cancel",
    "schedule.deliver",
];

pub fn parse(
    value: &Value,
    skill: &str,
    path: &str,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<Vec<Arc<Tool>>> {
    let root = value.as_object()?;
    let server = root.get("server").and_then(Value::as_str)?.to_owned();
    let list = root.get("tools").and_then(Value::as_array)?;
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
        if let Some(tool) = parse_tool(obj, skill, &server, path, &format!("/tools/{i}"), issues) {
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
    (issues.error_count() == before).then_some(tools)
}

fn parse_tool(
    obj: &Map<String, Value>,
    skill: &str,
    server: &str,
    path: &str,
    at: &str,
    issues: &mut Issues,
) -> Option<Tool> {
    let name = obj.get("id").and_then(Value::as_str)?.to_owned();
    if RESERVED_TOOL_IDS.contains(&format!("{skill}.{name}").as_str()) {
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
    let input = obj.get("input")?.clone();
    let before = issues.error_count();
    let schema = compile_input(&input, path, at, issues);
    let destination = parse_destination(obj.get("destination"), class, &input, path, at, issues);
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
    })
}

/// 인자는 항상 객체이므로 입력 스키마의 최상위는 `type: object`여야 하고, 지원하는 키워드만 써야 한다.
fn compile_input(input: &Value, path: &str, at: &str, issues: &mut Issues) -> Option<Schema> {
    let at = format!("{at}/input");
    if input.get("type") != Some(&json!("object")) {
        issues.error_at(
            "bad-input-schema",
            path,
            at,
            "input의 최상위는 `type: object`여야 해요",
        );
        return None;
    }
    match Schema::compile_input(input.clone()) {
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
mod tests {
    use super::*;

    fn parse_text(doc: Value) -> (Option<Vec<Arc<Tool>>>, Vec<&'static str>) {
        let mut issues = Issues::new(50);
        let parsed = parse(
            &doc,
            "agenda",
            "tools.yaml",
            &Limits::default(),
            &mut issues,
        );
        (parsed, issues.items().iter().map(|i| i.code).collect())
    }

    fn doc(tool: Value) -> Value {
        json!({"version": 1, "server": "demo", "tools": [tool]})
    }

    fn tool(extra: Value) -> Value {
        let mut base = json!({
            "id": "send", "class": "write", "scopes": ["mail.send"],
            "input": {"type": "object", "properties": {"to": {"type": "string"}, "message": {"type": "string"}}, "additionalProperties": false}
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    #[test]
    fn builds_tools_with_revision_and_schema() {
        let (parsed, codes) = parse_text(doc(tool(
            json!({"destination": {"recipient": "args.to", "content": "args.message"}, "egress": ["mail.test:443"]}),
        )));
        assert!(codes.is_empty(), "{codes:?}");
        let tools = parsed.unwrap();
        let t = &tools[0];
        assert_eq!(t.id.0, "agenda.send");
        assert_eq!(t.risk(), Risk::Irreversible);
        assert_eq!(t.revision.0.len(), 64);
        assert!(
            t.schema
                .validate(&json!({"to": "a", "message": "b"}))
                .is_empty()
        );
        assert!(!t.schema.validate(&json!({"to": 1})).is_empty());
        let (other, _) = parse_text(doc(tool(json!({"scopes": ["mail.other"]}))));
        assert_ne!(other.unwrap()[0].revision, t.revision);
    }

    #[test]
    fn rejects_bad_declarations() {
        assert_eq!(
            parse_text(doc(tool(
                json!({"destination": {"recipient": "args.nope", "content": "args.message"}})
            )))
            .1,
            ["bad-destination"]
        );
        assert_eq!(
            parse_text(doc(tool(json!({"class": "read", "destination": "reply"})))).1,
            ["destination-on-read"]
        );
        assert_eq!(
            parse_text(doc(tool(json!({"egress": ["a.test:0"]})))).1,
            ["bad-egress"]
        );
        assert_eq!(
            parse_text(doc(tool(json!({"egress": ["-a.test:443"]})))).1,
            ["bad-egress"]
        );
        assert_eq!(
            parse_text(doc(tool(json!({"egress": ["a..test:443"]})))).1,
            ["bad-egress"]
        );
        assert_eq!(
            parse_text(doc(tool(json!({"input": {"properties": {}}})))).1,
            ["bad-input-schema"]
        );
        assert_eq!(
            parse_text(doc(tool(
                json!({"input": {"type": "object", "patternProperties": {}}})
            )))
            .1,
            ["bad-input-schema"]
        );
        let dup = json!({"version": 1, "server": "s", "tools": [tool(json!({})), tool(json!({}))]});
        assert_eq!(parse_text(dup).1, ["duplicate-tool"]);
    }

    #[test]
    fn reply_destination_is_allowed_on_write_tools() {
        let (parsed, codes) = parse_text(doc(tool(json!({"destination": "reply"}))));
        assert!(codes.is_empty());
        assert!(matches!(
            parsed.unwrap()[0].destination,
            Some(Destination::Reply)
        ));
    }

    #[test]
    fn only_the_exact_builtin_tool_id_is_reserved() {
        let named = |skill: &str, tool: &str| {
            let doc = doc(tool_named(tool));
            let mut issues = Issues::new(10);
            let parsed = parse(&doc, skill, "tools.yaml", &Limits::default(), &mut issues);
            (
                parsed.is_some(),
                issues.items().iter().map(|i| i.code).collect::<Vec<_>>(),
            )
        };
        assert_eq!(named("memory", "share"), (false, vec!["reserved-tool"]));
        for tool in ["create", "list", "cancel", "deliver"] {
            assert_eq!(
                named("schedule", tool),
                (false, vec!["reserved-tool"]),
                "schedule.{tool}"
            );
        }
        for tool in ["get", "update", "delete"] {
            assert_eq!(named("schedule", tool), (true, vec![]), "schedule.{tool}");
        }
        assert_eq!(named("calendar", "create"), (true, vec![]));
        assert_eq!(named("schedule-notes", "create"), (true, vec![]));
        assert_eq!(
            named("memory", "recall"),
            (true, vec![]),
            "memory 스킬 자체는 허용"
        );
        assert_eq!(
            named("mail", "share"),
            (true, vec![]),
            "같은 도구 이름은 허용"
        );
        assert_eq!(named("memory-notes", "share"), (true, vec![]));
    }

    fn tool_named(name: &str) -> Value {
        json!({"id": name, "class": "read", "scopes": ["x.read"],
               "input": {"type": "object", "properties": {}, "additionalProperties": false}})
    }
}
