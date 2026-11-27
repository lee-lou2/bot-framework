//! Parsing and local validation of `triggers.yaml` (cross-file references are checked in `resolve`).
use crate::{
    issue::Issues,
    limits::Limits,
    model::{Match, Source, Trigger, TriggerTarget},
    names,
};
use bot_core::{
    cron::Cron,
    id::{BindingId, PrincipalId, ToolId},
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Valid top-level roots for dotted envelope paths in `match` and `input`.
pub const ENVELOPE_ROOTS: &[&str] = &[
    "request_id",
    "binding",
    "principal",
    "conversation",
    "attachments",
    "body",
];

pub fn parse(value: &Value, path: &str, limits: &Limits, issues: &mut Issues) -> Vec<Trigger> {
    let list = value
        .get("triggers")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if list.len() > limits.max_triggers {
        issues.error_at(
            "too-many",
            path,
            "/triggers",
            format!("too many triggers (max {})", limits.max_triggers),
        );
        return Vec::new();
    }
    let mut out: Vec<Trigger> = Vec::new();
    let mut seen = BTreeSet::new();
    for (i, item) in list.iter().enumerate() {
        let at = format!("/triggers/{i}");
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(trigger) = parse_one(obj, path, &at, issues) else {
            continue;
        };
        if !seen.insert(trigger.id.clone()) {
            issues.error_at(
                "duplicate-trigger-id",
                path,
                format!("{at}/id"),
                format!("duplicate trigger id: {}", trigger.id),
            );
            continue;
        }
        out.push(trigger);
    }
    out
}

fn text<'a>(map: &'a Map<String, Value>, key: &str) -> &'a str {
    map.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn parse_one(
    obj: &Map<String, Value>,
    path: &str,
    at: &str,
    issues: &mut Issues,
) -> Option<Trigger> {
    let before = issues.error_count();
    let on_map = obj.get("on")?.as_object()?;
    let target = parse_target(obj.get("target")?.as_object()?);
    let sync = obj
        .get("response")
        .and_then(|r| r.get("mode"))
        .and_then(Value::as_str)
        == Some("sync");
    if sync && matches!(target, TriggerTarget::Instruction(_)) {
        issues.error_at(
            "sync-needs-tool",
            path,
            format!("{at}/response"),
            "synchronous response (`sync`) returns a tool result directly and requires a tool target",
        );
    }
    let identity = |key: &str, issues: &mut Issues| -> Option<PrincipalId> {
        let who = obj.get(key).and_then(Value::as_str)?;
        if names::identity_kind(who).is_none() {
            issues.error_at(
                "bad-identity",
                path,
                format!("{at}/{key}"),
                format!("identity must have the form `<kind>:<name>`: {who}"),
            );
        }
        Some(PrincipalId(who.to_owned()))
    };
    let (principal, approver) = (identity("principal", issues), identity("approver", issues));
    let beat = obj.get("beat").and_then(Value::as_bool).unwrap_or(false);
    let on = parse_source(
        on_map,
        (principal, approver, beat),
        sync,
        path,
        &format!("{at}/on"),
        issues,
    )?;
    let matches = parse_matches(obj.get("match"), path, at, issues);
    let input = parse_input(obj.get("input"), path, at, issues);
    let declared = crate::approval::parse(obj, path, at, issues);
    (issues.error_count() == before).then(|| Trigger {
        id: text(obj, "id").to_owned(),
        on,
        target,
        order: obj
            .get("order")
            .and_then(crate::schema::as_exact_i64)
            .unwrap_or(0),
        private: obj.get("private").and_then(Value::as_bool).unwrap_or(false),
        private_pass: obj
            .get("private_pass")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        matches,
        input,
        preapproved: declared.preapproved,
        human_approval: declared.human_approval,
    })
}

fn parse_source(
    on: &Map<String, Value>,
    who: (Option<PrincipalId>, Option<PrincipalId>, bool),
    sync: bool,
    path: &str,
    at: &str,
    issues: &mut Issues,
) -> Option<Source> {
    if let Some(cron) = on.get("cron").and_then(Value::as_str) {
        let (principal, approver, beat) = who;
        return match Cron::parse(cron) {
            Ok(cron) => Some(Source::Cron {
                cron,
                beat,
                principal: principal?,
                approver,
            }),
            Err(e) => {
                issues.error_at("bad-cron", path, format!("{at}/cron"), e);
                None
            }
        };
    }
    let binding = BindingId(text(on, "binding").to_owned());
    if let Some(http) = on.get("http").and_then(Value::as_object) {
        let route = text(http, "path");
        if !http_path_ok(route) {
            issues.error_at(
                "bad-http-path",
                path,
                format!("{at}/http/path"),
                format!(
                    "invalid HTTP path (must start with `/`, no empty segments, `.`, `..`, or whitespace, max 256 bytes): {route}"
                ),
            );
            return None;
        }
        return Some(Source::Http {
            method: text(http, "method").to_owned(),
            path: route.to_owned(),
            binding,
            sync,
        });
    }
    match text(on, "event") {
        "message" => Some(Source::Message { binding }),
        "thread-reply" => Some(Source::ThreadReply { binding }),
        "reaction" => Some(Source::Reaction { binding }),
        event => {
            issues.error_at(
                "unsupported-event",
                path,
                format!("{at}/event"),
                format!("supported events are `message`, `thread-reply`, and `reaction`: {event}"),
            );
            None
        }
    }
}

fn http_path_ok(route: &str) -> bool {
    let body = route.strip_prefix('/').unwrap_or("!");
    route.len() <= 256
        && !route.chars().any(|c| c.is_whitespace() || c.is_control())
        && (body.is_empty()
            || body.split('/').enumerate().all(|(i, seg)| {
                let last = i == body.split('/').count() - 1;
                seg != "." && seg != ".." && (!seg.is_empty() || last && !body.ends_with("//"))
            }))
        && !route.contains("//")
        && route.starts_with('/')
}

fn parse_target(target: &Map<String, Value>) -> TriggerTarget {
    match target.get("tool").and_then(Value::as_str) {
        Some(tool) => TriggerTarget::Tool {
            tool: ToolId(tool.to_owned()),
            args: target
                .get("args")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        },
        None => TriggerTarget::Instruction(text(target, "instruction").to_owned()),
    }
}

fn path_of(dotted: &str, path: &str, at: &str, issues: &mut Issues) -> String {
    let root = dotted.split('.').next().unwrap_or_default();
    if !ENVELOPE_ROOTS.contains(&root) {
        issues.error_at(
            "bad-path",
            path,
            at,
            format!(
                "envelope path must start with one of {}: {dotted}",
                ENVELOPE_ROOTS.join(", ")
            ),
        );
    }
    dotted.to_owned()
}

fn parse_matches(value: Option<&Value>, path: &str, at: &str, issues: &mut Issues) -> Vec<Match> {
    let Some(map) = value.and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter()
        .map(|(key, literal)| Match {
            path: path_of(key, path, &format!("{at}/match"), issues),
            value: literal.clone(),
        })
        .collect()
}

fn parse_input(
    value: Option<&Value>,
    path: &str,
    at: &str,
    issues: &mut Issues,
) -> BTreeMap<String, String> {
    let Some(map) = value.and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    map.iter()
        .filter_map(|(arg, p)| {
            Some((
                arg.clone(),
                path_of(p.as_str()?, path, &format!("{at}/input/{arg}"), issues),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(doc: Value) -> (Vec<Trigger>, Vec<&'static str>) {
        let mut issues = Issues::new(50);
        let out = parse(&doc, "triggers.yaml", &Limits::default(), &mut issues);
        (out, issues.items().iter().map(|i| i.code).collect())
    }

    #[test]
    fn parses_the_three_source_kinds() {
        let (out, codes) = run(json!({"version": 1, "triggers": [
            {"id": "morning", "on": {"cron": "0 9 * * *"}, "principal": "service:morning", "target": {"tool": "agenda.list", "args": {}}},
            {"id": "chat", "on": {"event": "message", "binding": "team"}, "target": {"instruction": "summarize"}},
            {"id": "cancel", "on": {"http": {"method": "POST", "path": "/agenda"}, "binding": "inbox"}, "order": 10,
             "match": {"body.action": "cancel"}, "input": {"event": "body.event"}, "target": {"tool": "agenda.delete"}, "response": {"mode": "accepted"}}
        ]}));
        assert!(codes.is_empty(), "{codes:?}");
        assert!(
            matches!(&out[0].on, Source::Cron { principal, beat: false, .. } if principal.0 == "service:morning")
        );
        assert!(matches!(&out[1].on, Source::Message { binding } if binding.0 == "team"));
        assert_eq!(out[2].order, 10);
        assert_eq!(out[2].matches[0].path, "body.action");
        assert_eq!(out[2].input["event"], "body.event");
        assert_eq!(
            out[2].on.group().as_deref(),
            Some("http:inbox:POST /agenda")
        );
    }

    #[test]
    fn rejects_local_mistakes() {
        let base = |extra: Value| {
            let mut t = json!({"id": "t", "on": {"event": "message", "binding": "b"}, "target": {"instruction": "x"}});
            t.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({"version": 1, "triggers": [t]})
        };
        assert_eq!(
            run(base(
                json!({"on": {"cron": "61 * * * *"}, "principal": "service:a"})
            ))
            .1,
            ["bad-cron"]
        );
        assert_eq!(
            run(base(
                json!({"principal": "owner", "on": {"cron": "* * * * *"}})
            ))
            .1,
            ["bad-identity"]
        );
        assert_eq!(
            run(base(json!({"on": {"event": "edited", "binding": "b"}}))).1,
            ["unsupported-event"]
        );
        // `thread-reply` and `reaction` are parsed locally; binding `events` compatibility is checked in `resolve`.
        for event in ["thread-reply", "reaction"] {
            assert!(
                run(base(json!({"on": {"event": event, "binding": "b"}})))
                    .1
                    .is_empty(),
                "{event}"
            );
        }
        assert_eq!(
            run(base(
                json!({"on": {"http": {"method": "POST", "path": "/a/../b"}, "binding": "b"}})
            ))
            .1,
            ["bad-http-path"]
        );
        assert_eq!(
            run(base(json!({"match": {"secret.token": "x"}}))).1,
            ["bad-path"]
        );
        assert_eq!(run(base(json!({"on": {"http": {"method": "POST", "path": "/a"}, "binding": "b"}, "response": {"mode": "sync"}}))).1, ["sync-needs-tool"]);
        let dup = json!({"version": 1, "triggers": [
            {"id": "t", "on": {"event": "message", "binding": "b"}, "target": {"instruction": "x"}},
            {"id": "t", "on": {"event": "message", "binding": "b"}, "order": 1, "target": {"instruction": "x"}}]});
        assert_eq!(run(dup).1, ["duplicate-trigger-id"]);
    }

    #[test]
    fn beat_flag_is_read_for_cron() {
        let (out, codes) = run(json!({"version": 1, "triggers": [
            {"id": "tick", "on": {"cron": "*/5 * * * *"}, "principal": "service:tick", "beat": true, "target": {"tool": "agenda.list"}}]}));
        assert!(codes.is_empty(), "{codes:?}");
        assert!(matches!(out[0].on, Source::Cron { beat: true, .. }));
    }

    #[test]
    fn http_paths() {
        for ok in ["/", "/agenda", "/a/b-c_d", "/a/"] {
            assert!(http_path_ok(ok), "{ok}");
        }
        for bad in ["agenda", "/a//b", "/a/./b", "/a/..", "/a b", ""] {
            assert!(!http_path_ok(bad), "{bad}");
        }
    }

    #[test]
    fn float_integer_order_and_preapproved_max_per_day_are_accepted() {
        let (out, codes) = run(json!({"version": 1, "triggers": [{
            "id": "morning",
            "on": {"cron": "0 9 * * *"},
            "principal": "service:morning",
            "order": 5.0,
            "target": {"tool": "agenda.list", "args": {}},
            "preapproved": {
                "max_per_day": 5.0,
                "targets": [{"operation": "send", "target": "team"}]
            }
        }]}));
        assert!(codes.is_empty(), "{codes:?}");
        assert_eq!(out[0].order, 5);
        assert_eq!(out[0].preapproved.as_ref().unwrap().max_per_day, 5);
    }
}
