//! Deterministic offline demo model (`RunnerKind::Step`) for the agenda list/delete flow.
//!
//! Reads state solely from `request.exchanges` so identical inputs always yield the same step.

use super::{
    CallEnd, DoneCall, ModelError, ModelMeta, ModelRequest, ModelRunner, ModelStep, PendingCall,
    Role, RunnerKind, StepFuture, Usage, call_id,
};
use bot_core::{command::JsonPayload, id::ToolId};
use serde_json::{Value, json};

const LIST: &str = "agenda.list";
const DELETE: &str = "agenda.delete";
const HELP: &str =
    "I am the demo model. Try asking 'Show today's agenda' or 'Delete the walk event'.";

#[derive(Clone, Copy, Debug, Default)]
pub struct DemoModel;

impl ModelRunner for DemoModel {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }

    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            if *request.cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            Ok(decide(&request))
        })
    }

    fn meta(&self, _alias: &str) -> Option<ModelMeta> {
        Some(ModelMeta {
            provider: "demo",
            model: "demo".to_owned(),
        })
    }
}

fn say(text: impl Into<String>) -> ModelStep {
    ModelStep::Final {
        text: text.into(),
        usage: Usage::default(),
        outcome: Default::default(),
    }
}

/// Deterministic call ID from (run, exchange index, slot) so resumed runs match.
fn call(request: &ModelRequest, tool: &str, args: Value, text: &str) -> ModelStep {
    ModelStep::Calls {
        text: text.into(),
        calls: vec![PendingCall {
            call: call_id(&request.attempt.run, request.exchanges.len(), 0),
            tool: ToolId(tool.into()),
            args: JsonPayload(args.to_string()),
        }],
        usage: Usage::default(),
        measured: None,
        state: None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Intent {
    List,
    Delete,
    Other,
}

fn intent(text: &str) -> Intent {
    let t = text.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if any(&["delete", "remove"]) {
        Intent::Delete
    } else if any(&["agenda", "calendar", "schedule"]) {
        Intent::List
    } else {
        Intent::Other
    }
}

fn decide(request: &ModelRequest) -> ModelStep {
    let text = request
        .conversation
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.text.clone())
        .unwrap_or_default();
    let has = |id: &str| request.tools.iter().any(|t| t.id.0 == id);
    match intent(&text) {
        Intent::Other => say(HELP),
        _ if !has(LIST) => {
            say("Agenda tools are unavailable. Check the agenda skill and calendar.read scope.")
        }
        Intent::List => match request.exchanges.first() {
            None => call(request, LIST, json!({}), "Checking the agenda."),
            Some(first) => say(render_list(first.calls.first())),
        },
        Intent::Delete => delete_flow(request, &text, has(DELETE)),
    }
}

fn delete_flow(request: &ModelRequest, text: &str, can_delete: bool) -> ModelStep {
    let exchanges = &request.exchanges;
    let Some(listed) = exchanges.first() else {
        return call(request, LIST, json!({}), "Looking up the event to delete.");
    };
    let events = match parse(listed.calls.first()) {
        Parsed::Events(events) => events,
        Parsed::Text(_) | Parsed::Unavailable => {
            return say("Could not load the agenda, so nothing was deleted.");
        }
    };
    let target = match pick(&events, text) {
        Pick::One(event) => event,
        Pick::None => {
            return say(format!(
                "Could not find that event.\n{}",
                render_events(&events)
            ));
        }
        Pick::Many(titles) => {
            return say(format!(
                "Multiple events match: {}. Which event should I delete?",
                titles.join(", ")
            ));
        }
    };
    match exchanges.get(1).and_then(|e| e.calls.first()) {
        None if !can_delete => say("Delete tool is unavailable. Check the calendar.delete scope."),
        None => call(
            request,
            DELETE,
            json!({"event": target.id}),
            &format!("Deleting '{}'.", target.title),
        ),
        Some(DoneCall {
            end: CallEnd::Done(payload),
            ..
        }) => {
            // Never report simulated server responses as permanent deletions.
            let simulated = serde_json::from_str::<Value>(&payload.0)
                .ok()
                .and_then(|v| simulation(&v));
            match simulated {
                Some(Some(receipt)) => say(format!(
                    "Simulated deletion of '{}' (virtual receipt {receipt}).",
                    target.title
                )),
                Some(None) => say(format!(
                    "Simulated deletion of '{}' (not permanently deleted).",
                    target.title
                )),
                None => say(format!("Deleted '{}'.", target.title)),
            }
        }
        Some(DoneCall {
            end: CallEnd::Denied,
            ..
        }) => say(format!(
            "Deletion was not approved, so '{}' was kept.",
            target.title
        )),
        Some(DoneCall {
            end: CallEnd::Failed(_),
            ..
        }) => say("Could not delete the event. Please try again shortly."),
    }
}

struct Event {
    id: String,
    title: String,
    when: Option<String>,
}

enum Parsed {
    Events(Vec<Event>),
    Text(String),
    Unavailable,
}

fn event_of(v: &Value) -> Option<Event> {
    let obj = v.as_object()?;
    let id = match obj.get("id")? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| obj.get(*k)?.as_str())
            .map(str::to_owned)
    };
    Some(Event {
        id,
        title: text(&["title", "name", "summary", "subject"])?,
        when: text(&["start", "at", "time", "when", "date"]),
    })
}

/// Extract events from arrays, common wrapper keys, or MCP `structuredContent`/`content` JSON blocks.
fn collect(v: &Value) -> Option<Vec<Event>> {
    match v {
        Value::Array(items) => {
            let events: Vec<Event> = items.iter().filter_map(event_of).collect();
            (items.is_empty() || !events.is_empty()).then_some(events)
        }
        Value::Object(map) => {
            if let Some(inner) = map.get("structuredContent") {
                return collect(inner);
            }
            for key in ["events", "items", "agenda", "results"] {
                if let Some(events) = map.get(key).and_then(collect) {
                    return Some(events);
                }
            }
            if let Some(Value::Array(blocks)) = map.get("content") {
                for block in blocks {
                    let inner = block
                        .get("text")
                        .and_then(Value::as_str)
                        .and_then(|t| serde_json::from_str::<Value>(t).ok());
                    if let Some(events) = inner.as_ref().and_then(collect) {
                        return Some(events);
                    }
                }
            }
            event_of(v).map(|e| vec![e])
        }
        _ => None,
    }
}

/// Detect simulated delete responses (`simulated: true`) and return any sanitized virtual receipt.
fn simulation(v: &Value) -> Option<Option<String>> {
    let map = v.as_object()?;
    if map.get("simulated").and_then(Value::as_bool) == Some(true) {
        return Some(
            map.get("receipt")
                .and_then(Value::as_str)
                .map(|r| r.chars().filter(|c| !c.is_control()).take(64).collect()),
        );
    }
    if let Some(found) = map.get("structuredContent").and_then(simulation) {
        return Some(found);
    }
    let Some(Value::Array(blocks)) = map.get("content") else {
        return None;
    };
    blocks.iter().find_map(|block| {
        let inner = serde_json::from_str::<Value>(block.get("text")?.as_str()?).ok()?;
        simulation(&inner)
    })
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(map) => match map.get("content") {
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => v.to_string(),
        },
        _ => v.to_string(),
    }
}

fn parse(call: Option<&DoneCall>) -> Parsed {
    let Some(DoneCall {
        end: CallEnd::Done(payload),
        ..
    }) = call
    else {
        return Parsed::Unavailable;
    };
    match serde_json::from_str::<Value>(&payload.0) {
        Ok(value) => collect(&value).map_or_else(|| Parsed::Text(plain(&value)), Parsed::Events),
        Err(_) => Parsed::Text(payload.0.trim().to_owned()),
    }
}

fn render_events(events: &[Event]) -> String {
    if events.is_empty() {
        return "No events found.".into();
    }
    let lines: Vec<String> = events
        .iter()
        .map(|e| match &e.when {
            Some(when) => format!("- {when} {}", e.title),
            None => format!("- {}", e.title),
        })
        .collect();
    format!("Found {} event(s).\n{}", events.len(), lines.join("\n"))
}

fn render_list(call: Option<&DoneCall>) -> String {
    match parse(call) {
        Parsed::Events(events) => render_events(&events),
        Parsed::Text(text) if text.is_empty() => "No events found.".into(),
        Parsed::Text(text) => text,
        Parsed::Unavailable => match call.map(|c| &c.end) {
            Some(CallEnd::Denied) => "Permission denied to read the agenda.".into(),
            _ => "Could not fetch the agenda. Please try again shortly.".into(),
        },
    }
}

enum Pick<'a> {
    One(&'a Event),
    None,
    Many(Vec<String>),
}

/// Match event titles mentioned in user text, preferring more specific titles when nested.
fn pick<'a>(events: &'a [Event], text: &str) -> Pick<'a> {
    let said = text.to_lowercase();
    let hits: Vec<&Event> = events
        .iter()
        .filter(|e| !e.title.trim().is_empty() && said.contains(&e.title.trim().to_lowercase()))
        .collect();
    let specific: Vec<&Event> = hits
        .iter()
        .copied()
        .filter(|e| {
            !hits
                .iter()
                .any(|o| o.title.len() > e.title.len() && o.title.contains(&e.title))
        })
        .collect();
    match specific.as_slice() {
        [] => Pick::None,
        [one] => Pick::One(one),
        many => Pick::Many(many.iter().map(|e| e.title.clone()).collect()),
    }
}

#[cfg(test)]
mod tests;
