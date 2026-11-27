//! Strict OpenAI Responses API SSE and JSON observer extracting `function_call` tool plans.

use super::{MAX_INPUT, Observed, ObservedUse, id_ok, observed};
use crate::json::parse;
use bot_core::{cli::codex_response_id, port::Error};
use serde_json::Value;
use std::collections::BTreeMap;

const TRACKED: [&str; 8] = [
    "response.created",
    "response.output_item.added",
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
    "response.output_item.done",
    "response.completed",
    "response.incomplete",
    "response.failed",
];

struct OpenCall {
    call_id: String,
    name: String,
    deltas: String,
    done_args: Option<String>,
}

enum Slot {
    Other,
    Call(OpenCall),
    Closed(Option<ObservedUse>),
}

#[derive(Default)]
pub(in crate::front) struct ResponsesObserver {
    started: bool,
    slots: BTreeMap<u64, Slot>,
    stopped: bool,
    aborted: bool,
    broken: bool,
}

/// Normalizes Codex namespace + function name into `mcp__relay__<tool>` when namespaced.
fn full_name(item: &Value) -> Option<String> {
    let name = item.get("name").and_then(Value::as_str)?;
    if name.is_empty() {
        return None;
    }
    match item.get("namespace").and_then(Value::as_str) {
        Some(ns) if !ns.is_empty() => Some(format!("{ns}__{name}")),
        _ => Some(name.to_owned()),
    }
}

fn parse_object_args(raw: &str) -> Option<String> {
    if raw.len() > MAX_INPUT {
        return None;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some("{}".to_owned());
    }
    match parse(trimmed)? {
        value @ Value::Object(_) => Some(value.to_string()),
        _ => None,
    }
}

impl ResponsesObserver {
    pub(super) fn feed(&mut self, event: &[u8]) {
        let Ok(text) = std::str::from_utf8(event) else {
            self.broken = true;
            return;
        };
        let (mut name, mut data) = (None, None);
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                if name
                    .replace(rest.strip_prefix(' ').unwrap_or(rest))
                    .is_some()
                {
                    self.broken = true;
                    return;
                }
            } else if let Some(rest) = line.strip_prefix("data:")
                && data
                    .replace(rest.strip_prefix(' ').unwrap_or(rest))
                    .is_some()
            {
                self.broken = true;
                return;
            }
        }
        let Some(data) = data else {
            return;
        };
        let Some(value) = parse(data) else {
            self.broken = true;
            return;
        };
        self.handle_event(name, &value);
    }

    fn handle_event(&mut self, name: Option<&str>, value: &Value) {
        let kind = value.get("type").and_then(Value::as_str);
        let tracked = name.is_some_and(|n| TRACKED.contains(&n))
            || kind.is_some_and(|k| TRACKED.contains(&k));
        if tracked && name != kind {
            self.broken = true;
            return;
        }
        let index = value.get("output_index").and_then(Value::as_u64);
        match kind {
            Some("response.created") => {
                if self.started {
                    self.broken = true;
                    return;
                }
                self.started = true;
            }
            Some("response.output_item.added") => self.added(index, value.get("item")),
            Some("response.function_call_arguments.delta") => {
                self.args_delta(index, value.get("delta").and_then(Value::as_str));
            }
            Some("response.function_call_arguments.done") => {
                self.args_done(index, value.get("arguments").and_then(Value::as_str));
            }
            Some("response.output_item.done") => self.item_done(index, value.get("item")),
            Some("response.completed") => {
                self.verify_completed(value.get("response"));
                self.stopped = true;
            }
            Some("response.incomplete" | "response.failed") => {
                self.aborted = true;
                self.stopped = true;
            }
            _ => {}
        }
    }

    fn added(&mut self, index: Option<u64>, item: Option<&Value>) {
        let (Some(index), Some(item)) = (index, item) else {
            self.broken = true;
            return;
        };
        if self.slots.contains_key(&index) {
            self.broken = true;
            return;
        }
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            self.slots.insert(index, Slot::Other);
            return;
        }
        let (Some(call_id), Some(name)) = (
            item.get("call_id")
                .and_then(Value::as_str)
                .filter(|id| id_ok(id)),
            full_name(item),
        ) else {
            self.broken = true;
            return;
        };
        let initial = match item.get("arguments") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) if s.len() <= MAX_INPUT => s.clone(),
            _ => {
                self.broken = true;
                return;
            }
        };
        self.slots.insert(
            index,
            Slot::Call(OpenCall {
                call_id: call_id.to_owned(),
                name,
                deltas: initial,
                done_args: None,
            }),
        );
    }

    fn args_delta(&mut self, index: Option<u64>, part: Option<&str>) {
        let (Some(index), Some(part)) = (index, part) else {
            self.broken = true;
            return;
        };
        match self.slots.get_mut(&index) {
            Some(Slot::Call(open)) if open.deltas.len() + part.len() <= MAX_INPUT => {
                open.deltas.push_str(part);
            }
            _ => self.broken = true,
        }
    }

    fn args_done(&mut self, index: Option<u64>, args: Option<&str>) {
        let (Some(index), Some(args)) = (index, args) else {
            self.broken = true;
            return;
        };
        if args.len() > MAX_INPUT {
            self.broken = true;
            return;
        }
        match self.slots.get_mut(&index) {
            Some(Slot::Call(open)) => {
                if !open.deltas.is_empty() && open.deltas != args {
                    self.broken = true;
                    return;
                }
                open.done_args = Some(args.to_owned());
            }
            _ => self.broken = true,
        }
    }

    fn item_done(&mut self, index: Option<u64>, item: Option<&Value>) {
        let (Some(index), Some(item)) = (index, item) else {
            self.broken = true;
            return;
        };
        let is_call = item.get("type").and_then(Value::as_str) == Some("function_call");
        let closed = match self.slots.remove(&index) {
            None => {
                // Allow a direct `output_item.done` without a preceding `added` event.
                if !is_call {
                    None
                } else {
                    match extract_item_call(item) {
                        Some(used) => Some(used),
                        None => {
                            self.broken = true;
                            return;
                        }
                    }
                }
            }
            Some(Slot::Closed(_)) => {
                self.broken = true;
                return;
            }
            Some(Slot::Other) => {
                if is_call {
                    self.broken = true;
                    return;
                }
                None
            }
            Some(Slot::Call(open)) => {
                if !is_call {
                    self.broken = true;
                    return;
                }
                let Some(done_call_id) = item.get("call_id").and_then(Value::as_str) else {
                    self.broken = true;
                    return;
                };
                let Some(done_name) = full_name(item) else {
                    self.broken = true;
                    return;
                };
                if done_call_id != open.call_id || done_name != open.name {
                    self.broken = true;
                    return;
                }
                let raw = match item.get("arguments").and_then(Value::as_str) {
                    Some(item_args) => {
                        if let Some(ref done) = open.done_args
                            && done != item_args
                        {
                            self.broken = true;
                            return;
                        }
                        if !open.deltas.is_empty() && open.deltas != item_args {
                            self.broken = true;
                            return;
                        }
                        item_args
                    }
                    None => open.done_args.as_deref().unwrap_or(open.deltas.as_str()),
                };
                match parse_object_args(raw) {
                    Some(input) => Some(ObservedUse {
                        id: open.call_id,
                        name: open.name,
                        input,
                    }),
                    None => {
                        self.broken = true;
                        return;
                    }
                }
            }
        };
        self.slots.insert(index, Slot::Closed(closed));
    }

    fn verify_completed(&mut self, response: Option<&Value>) {
        let Some(output) = response
            .and_then(|r| r.get("output"))
            .and_then(Value::as_array)
        else {
            return;
        };
        let mut expected = Vec::new();
        for item in output {
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                continue;
            }
            match extract_item_call(item) {
                Some(used) => expected.push(used),
                None => {
                    self.broken = true;
                    return;
                }
            }
        }
        let seen: Vec<&ObservedUse> = self
            .slots
            .values()
            .filter_map(|slot| match slot {
                Slot::Closed(Some(u)) => Some(u),
                _ => None,
            })
            .collect();
        if !seen.is_empty()
            && (seen.len() != expected.len() || seen.iter().zip(&expected).any(|(a, b)| *a != b))
        {
            self.broken = true;
        } else if seen.is_empty() && !expected.is_empty() {
            // Stream omitted `output_item.done` for function calls; reject ambiguous framing.
            self.broken = true;
        }
    }

    pub(super) fn stopped(&self) -> bool {
        self.stopped
    }

    pub(super) fn broken(&self) -> bool {
        self.broken
    }

    pub(super) fn finish(&mut self) -> Result<Option<Observed>, Error> {
        if self.aborted {
            return Ok(None);
        }
        let unfinished = self
            .slots
            .values()
            .any(|slot| !matches!(slot, Slot::Closed(_)));
        if self.broken || unfinished || !self.started {
            return Err(Error::Invalid);
        }
        let uses: Vec<ObservedUse> = std::mem::take(&mut self.slots)
            .into_values()
            .filter_map(|slot| match slot {
                Slot::Closed(Some(used)) => Some(used),
                _ => None,
            })
            .collect();
        if uses.is_empty() {
            return Ok(None);
        }
        let id = codex_response_id(&uses[0].id);
        observed(Some(id), uses)
    }
}

fn extract_item_call(item: &Value) -> Option<ObservedUse> {
    let id = item
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| id_ok(id))?;
    let name = full_name(item)?;
    let raw = item.get("arguments").and_then(Value::as_str)?;
    let input = parse_object_args(raw)?;
    Some(ObservedUse {
        id: id.to_owned(),
        name,
        input,
    })
}

pub(super) fn observe_body(body: &str) -> Result<Option<Observed>, Error> {
    let Some(value) = parse(body) else {
        return Err(Error::Invalid);
    };
    let Some(items) = value.get("output").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut uses = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        let used = extract_item_call(item).ok_or(Error::Invalid)?;
        uses.push(used);
    }
    if uses.is_empty() {
        return Ok(None);
    }
    let id = codex_response_id(&uses[0].id);
    observed(Some(id), uses)
}
