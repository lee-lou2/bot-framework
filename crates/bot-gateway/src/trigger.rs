//! Trigger matching and literal dotted-path input mapping without expression or template evaluation.
use crate::{
    inbound::{Event, Inbound, kind_name},
    routes::BoundTrigger,
};
use bot_core::{
    id::{BindingId, ProfileId},
    profile::{Trigger, TriggerSource, TriggerTarget},
};
use serde_json::{Map, Value, json};

/// Builds the condition/input envelope without exposing profile or revision identifiers.
pub fn envelope_json(binding: &BindingId, inbound: &Inbound) -> Value {
    json!({
        "request_id": inbound.request.0,
        "binding": binding.0,
        "principal": inbound.principal.0,
        "conversation": {
            "kind": kind_name(&inbound.conversation.kind),
            "id": inbound.conversation.id,
            "thread": inbound.conversation.thread,
        },
        "attachments": inbound.attachments,
        "body": inbound.body,
    })
}

pub fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |current, key| current.as_object()?.get(key))
}

fn conditions_hold(trigger: &Trigger, envelope: &Value) -> bool {
    trigger.conditions.iter().all(|(path, literal)| {
        serde_json::from_str::<Value>(&literal.0)
            .ok()
            .is_some_and(|want| lookup(envelope, path) == Some(&want))
    })
}

fn event_name(event: &Event) -> Option<&'static str> {
    match event {
        Event::Message => Some("message"),
        Event::ThreadReply => Some("thread-reply"),
        Event::Reaction => Some("reaction"),
        Event::Http { .. } => None,
    }
}

pub fn applies(trigger: &Trigger, binding: &BindingId, event: &Event) -> bool {
    match (&trigger.on, event) {
        (
            TriggerSource::Event {
                binding: on,
                event: name,
            },
            Event::Message | Event::ThreadReply | Event::Reaction,
        ) => on == binding && Some(name.as_str()) == event_name(event),
        (
            TriggerSource::Http {
                binding: on,
                method,
                path,
                ..
            },
            Event::Http {
                method: got_method,
                path: got_path,
            },
        ) => on == binding && method == got_method && path == got_path,
        _ => false,
    }
}

/// Returns the first matching trigger; callers pass `triggers` pre-sorted by `order` from `Routes`.
pub fn select<'a>(
    triggers: &'a [BoundTrigger],
    binding: &BindingId,
    event: &Event,
    envelope: &Value,
) -> Option<&'a BoundTrigger> {
    triggers.iter().find(|bound| {
        applies(&bound.trigger, binding, event) && conditions_hold(&bound.trigger, envelope)
    })
}

pub fn select_all<'a>(
    triggers: &'a [BoundTrigger],
    binding: &BindingId,
    event: &Event,
    envelope: &Value,
) -> Vec<&'a BoundTrigger> {
    triggers
        .iter()
        .filter(|bound| {
            applies(&bound.trigger, binding, event) && conditions_hold(&bound.trigger, envelope)
        })
        .collect()
}

/// Merges static tool args with mapped envelope paths into canonical compact JSON; fails on key collision or missing path.
pub fn tool_args(trigger: &Trigger, envelope: &Value) -> Option<String> {
    let TriggerTarget::Tool { args, .. } = &trigger.target else {
        return None;
    };
    let mut object: Map<String, Value> = match serde_json::from_str(&args.0).ok()? {
        Value::Object(map) => map,
        _ => return None,
    };
    for (name, path) in &trigger.input {
        if object.contains_key(name) {
            return None;
        }
        object.insert(name.clone(), lookup(envelope, path)?.clone());
    }
    Some(Value::Object(object).to_string())
}

pub fn label(profile: &ProfileId, trigger: &Trigger) -> String {
    format!("{}/{}", profile.0, trigger.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use bot_core::profile::TriggerTarget;

    fn env() -> Value {
        json!({"body": {"action": "cancel", "event": "e-1", "n": 3, "nested": {"ok": true}},
               "conversation": {"kind": "http"}})
    }

    #[test]
    fn looks_up_dotted_object_paths_only() {
        let env = env();
        assert_eq!(lookup(&env, "body.action"), Some(&json!("cancel")));
        assert_eq!(lookup(&env, "body.nested.ok"), Some(&json!(true)));
        assert_eq!(lookup(&env, "body.missing"), None);
        assert_eq!(lookup(&env, "body.action.deeper"), None);
        assert_eq!(lookup(&env, ""), None);
    }

    fn with_conditions(pairs: &[(&str, &str)]) -> Trigger {
        let mut t = http_trigger(
            "t",
            "inbox",
            "POST",
            "/agenda",
            tool_target("agenda.delete", "{}"),
        );
        for (path, literal) in pairs {
            t.conditions.insert(
                (*path).into(),
                bot_core::command::JsonPayload((*literal).into()),
            );
        }
        t
    }

    #[test]
    fn conditions_are_literal_equality_and_all_must_hold() {
        let env = env();
        assert!(conditions_hold(&with_conditions(&[]), &env));
        assert!(conditions_hold(
            &with_conditions(&[("body.action", "\"cancel\"")]),
            &env
        ));
        assert!(conditions_hold(
            &with_conditions(&[
                ("body.action", "\"cancel\""),
                ("body.n", "3"),
                ("body.nested.ok", "true")
            ]),
            &env
        ));
        assert!(!conditions_hold(
            &with_conditions(&[("body.action", "\"other\"")]),
            &env
        ));
        assert!(!conditions_hold(
            &with_conditions(&[("body.action", "\"cancel\""), ("body.n", "4")]),
            &env
        ));
        assert!(!conditions_hold(
            &with_conditions(&[("body.n", "\"3\"")]),
            &env
        ));
        assert!(!conditions_hold(
            &with_conditions(&[("body.gone", "null")]),
            &env
        ));
        assert!(!conditions_hold(
            &with_conditions(&[("body.action", "not json")]),
            &env
        ));
    }

    fn bound(id: &str, order: i64, conditions: &[(&str, &str)]) -> BoundTrigger {
        let mut trigger = with_conditions(conditions);
        trigger.id = id.into();
        trigger.order = order;
        BoundTrigger {
            agent: bot_core::id::AgentId("main".into()),
            trigger,
        }
    }

    #[test]
    fn selects_the_first_match_by_order_for_the_same_method_and_path() {
        let list = vec![
            bound("cancel", 10, &[("body.action", "\"cancel\"")]),
            bound("fallback", 20, &[]),
        ];
        let binding = BindingId("inbox".into());
        let event = Event::Http {
            method: "POST".into(),
            path: "/agenda".into(),
        };
        let pick = |e: &Value| select(&list, &binding, &event, e).map(|b| b.trigger.id.clone());
        assert_eq!(pick(&env()).as_deref(), Some("cancel"));
        assert_eq!(
            pick(&json!({"body": {"action": "list"}})).as_deref(),
            Some("fallback")
        );
        let other = Event::Http {
            method: "GET".into(),
            path: "/agenda".into(),
        };
        assert!(select(&list, &binding, &other, &env()).is_none());
        assert!(select(&list, &BindingId("x".into()), &event, &env()).is_none());
        assert!(select(&list, &binding, &Event::Message, &env()).is_none());
    }

    #[test]
    fn maps_input_paths_into_args_and_refuses_ambiguous_or_missing_ones() {
        let mut trigger = with_conditions(&[]);
        trigger.target = tool_target("agenda.delete", r#"{"dry_run": false}"#);
        trigger.input.insert("event".into(), "body.event".into());
        let args = tool_args(&trigger, &env()).unwrap();
        assert_eq!(args, r#"{"dry_run":false,"event":"e-1"}"#);
        trigger.input.insert("count".into(), "body.n".into());
        assert_eq!(
            tool_args(&trigger, &env()).unwrap(),
            r#"{"count":3,"dry_run":false,"event":"e-1"}"#
        );
        trigger.input.insert("gone".into(), "body.gone".into());
        assert!(tool_args(&trigger, &env()).is_none());
        trigger.input.remove("gone");
        trigger.input.insert("dry_run".into(), "body.event".into());
        assert!(tool_args(&trigger, &env()).is_none());
        let mut bad = with_conditions(&[]);
        bad.target = tool_target("agenda.delete", "[]");
        assert!(tool_args(&bad, &env()).is_none());
        bad.target = TriggerTarget::Instruction("x".into());
        assert!(tool_args(&bad, &env()).is_none());
    }
}
