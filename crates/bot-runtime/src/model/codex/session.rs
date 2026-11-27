//! Pure Codex rollout JSONL parsing and broker-verified checkpoint journaling state machine.

use crate::model::session::{MAX_LINE_BYTES, Pending, ToolUse, rec_step};
use bot_core::{
    canon::parse as strict_parse,
    cli::{CliGroup, CliResponse, CliUse, GROUP_PREFIX, USE_PREFIX, codex_response_id},
    command::{JsonPayload, Step},
};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_BUFFERED_BYTES: usize = 16 << 20;
const MAX_BUFFERED_LINES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Why {
    Unsaved,
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refused {
    NotYet,
    Incomplete,
    Mismatch,
    Unverified,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Known {
    pub turn: u64,
    pub pos: u32,
    pub name: String,
    pub input: Value,
}

struct Line {
    seq: u64,
    raw: String,
    value: Value,
}

struct Located {
    seq: u64,
    tool: ToolUse,
}

struct Open {
    id: String,
    turn: u64,
    uses: Vec<Located>,
    closed: bool,
}

pub(crate) struct Plan {
    pub steps: Vec<Step>,
    pub verify: Vec<Pending>,
    through: u64,
}

fn kind(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn is_rollout_record(v: &Value) -> bool {
    v.is_object()
        && matches!(
            kind(v),
            "session_meta" | "response_item" | "turn_context" | "event_msg" | "compacted"
        )
}

pub(crate) fn thread_id_valid(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

pub(crate) fn session_thread_id(v: &Value) -> Option<&str> {
    (kind(v) == "session_meta")
        .then(|| v.get("payload")?.get("id")?.as_str())
        .flatten()
        .filter(|id| thread_id_valid(id))
}

fn full_name(payload: &Value) -> Option<String> {
    let name = payload.get("name").and_then(Value::as_str)?;
    if name.is_empty() {
        return None;
    }
    match payload.get("namespace").and_then(Value::as_str) {
        Some(ns) if !ns.is_empty() => Some(format!("{ns}__{name}")),
        _ => Some(name.to_owned()),
    }
}

fn parse_arguments(payload: &Value) -> Option<Value> {
    let raw = payload.get("arguments")?.as_str()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(Value::Object(Default::default()));
    }
    match strict_parse(trimmed)? {
        val @ Value::Object(_) => Some(val),
        _ => None,
    }
}

pub(crate) enum CallParse {
    None,
    Valid(ToolUse),
    Malformed,
}

pub(crate) fn parse_function_call(v: &Value) -> CallParse {
    if kind(v) != "response_item" {
        return CallParse::None;
    }
    let Some(payload) = v.get("payload") else {
        return CallParse::None;
    };
    if payload.get("type").and_then(Value::as_str) != Some("function_call") {
        return CallParse::None;
    }
    let (Some(id), Some(name), Some(input)) = (
        payload
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
        full_name(payload),
        parse_arguments(payload),
    ) else {
        return CallParse::Malformed;
    };
    CallParse::Valid(ToolUse {
        id: id.to_owned(),
        name,
        input,
    })
}

pub(crate) fn function_call_output_id(v: &Value) -> Option<&str> {
    if kind(v) != "response_item" {
        return None;
    }
    let payload = v.get("payload")?;
    (payload.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .then(|| payload.get("call_id")?.as_str())
        .flatten()
}

fn closes_turn(v: &Value) -> bool {
    match kind(v) {
        "turn_context" => true,
        "response_item" => {
            let Some(payload) = v.get("payload") else {
                return false;
            };
            match payload.get("type").and_then(Value::as_str) {
                Some("function_call_output") => true,
                Some("message") => payload.get("role").and_then(Value::as_str) == Some("user"),
                _ => false,
            }
        }
        _ => false,
    }
}

pub(crate) struct Journal {
    next: u64,
    buffer: Vec<Line>,
    open: Vec<Open>,
    buffered: usize,
    known: BTreeMap<String, Known>,
    relay_prefix: String,
    frozen: Option<Why>,
    blocked: bool,
    malformed: bool,
    saved: Vec<Pending>,
}

impl Journal {
    pub(crate) fn new(next: u64, known: BTreeMap<String, Known>, relay_prefix: String) -> Self {
        Self {
            next,
            buffer: Vec::new(),
            open: Vec::new(),
            buffered: 0,
            known,
            relay_prefix,
            frozen: None,
            blocked: false,
            malformed: false,
            saved: Vec::new(),
        }
    }

    pub(crate) fn stuck(&self) -> Option<Why> {
        self.frozen.filter(|_| self.blocked)
    }

    pub(crate) fn malformed(&self) -> bool {
        self.malformed
    }

    pub(crate) fn saved_relay(&self) -> &[Pending] {
        &self.saved
    }

    pub(crate) fn unsaved_relay(&self) -> bool {
        self.open
            .iter()
            .flat_map(|o| &o.uses)
            .any(|u| u.tool.name.starts_with(&self.relay_prefix))
    }

    pub(crate) fn freeze(&mut self, why: Why) {
        self.blocked |= self.unsaved_relay();
        self.frozen.get_or_insert(why);
        self.buffer.clear();
        self.open.clear();
        self.buffered = 0;
    }

    pub(crate) fn oldest(&self) -> Option<&str> {
        self.open.first().map(|o| o.id.as_str())
    }

    pub(crate) fn feed(&mut self, raw: &str) {
        if self.frozen.is_some() {
            if !self.blocked
                && raw.contains("\"function_call\"")
                && let Ok(v) = serde_json::from_str::<Value>(raw)
            {
                match parse_function_call(&v) {
                    CallParse::Valid(u) if u.name.starts_with(&self.relay_prefix) => {
                        self.blocked = true;
                    }
                    CallParse::Malformed => self.malformed = true,
                    _ => {}
                }
            }
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return;
        };
        if !is_rollout_record(&value) {
            return;
        }
        if raw.len() > MAX_LINE_BYTES
            || self.buffered + raw.len() > MAX_BUFFERED_BYTES
            || self.buffer.len() >= MAX_BUFFERED_LINES
        {
            self.freeze(Why::TooLarge);
            match parse_function_call(&value) {
                CallParse::Valid(u) if u.name.starts_with(&self.relay_prefix) => {
                    self.blocked = true;
                }
                CallParse::Malformed => self.malformed = true,
                _ => {}
            }
            return;
        }
        self.buffered += raw.len();
        let seq = self.next;
        self.next += 1;
        match parse_function_call(&value) {
            CallParse::Valid(tool) => {
                if let Some(last) = self.open.last_mut().filter(|o| !o.closed) {
                    last.uses.push(Located { seq, tool });
                } else {
                    let id = codex_response_id(&tool.id);
                    self.open.push(Open {
                        id,
                        turn: seq,
                        uses: vec![Located { seq, tool }],
                        closed: false,
                    });
                }
            }
            CallParse::Malformed => {
                self.malformed = true;
            }
            CallParse::None => {
                if closes_turn(&value)
                    && let Some(last) = self.open.last_mut()
                {
                    last.closed = true;
                }
            }
        }
        self.buffer.push(Line {
            seq,
            raw: raw.to_owned(),
            value,
        });
    }

    pub(crate) fn plan(&self, response: &CliResponse) -> Result<Plan, Refused> {
        let Some(open) = self.open.first() else {
            return Err(Refused::Mismatch);
        };
        if open.id != response.id {
            return Err(if self.open.iter().any(|o| o.id == response.id) {
                Refused::NotYet
            } else {
                Refused::Mismatch
            });
        }
        if !response.is_valid() || open.uses.len() > response.uses.len() {
            return Err(Refused::Mismatch);
        }
        for (got, want) in open.uses.iter().zip(&response.uses) {
            if got.tool.id != want.id || got.tool.name != want.name || got.tool.input != want.input
            {
                return Err(Refused::Mismatch);
            }
        }
        if open.uses.len() < response.uses.len() {
            return Err(Refused::Incomplete);
        }
        let end = self.open.get(1).map_or(u64::MAX, |next| next.turn);
        let included: Vec<&Line> = self.buffer.iter().filter(|l| l.seq < end).collect();
        let mut steps: Vec<Step> = included.iter().map(|l| rec_step(l.seq, &l.raw)).collect();
        let mut verify = Vec::new();
        for line in &included {
            let Some(id) = function_call_output_id(&line.value) else {
                continue;
            };
            let place = self.known.get(id).map(|k| Pending {
                turn: k.turn,
                pos: k.pos,
                tool_use: ToolUse {
                    id: id.to_owned(),
                    name: k.name.clone(),
                    input: k.input.clone(),
                },
            });
            let place = place.or_else(|| {
                let pos = open.uses.iter().position(|u| u.tool.id == id)?;
                Some(Pending {
                    turn: open.turn,
                    pos: pos as u32,
                    tool_use: open.uses[pos].tool.clone(),
                })
            });
            let Some(place) = place else {
                return Err(Refused::Unverified);
            };
            if place.tool_use.name.starts_with(&self.relay_prefix) {
                verify.push(place);
            }
        }
        for (pos, u) in open.uses.iter().enumerate() {
            steps.push(step(
                format!("{USE_PREFIX}{}", u.tool.id),
                serde_json::to_value(CliUse {
                    seq: u.seq,
                    turn: open.turn,
                    pos: pos as u32,
                    name: u.tool.name.clone(),
                    input: u.tool.input.clone(),
                })
                .unwrap_or_default(),
            ));
        }
        steps.push(step(
            format!("{GROUP_PREFIX}{}", open.turn),
            serde_json::to_value(CliGroup {
                seq: open.turn,
                uses: open.uses.iter().map(|u| u.tool.id.clone()).collect(),
            })
            .unwrap_or_default(),
        ));
        Ok(Plan {
            steps,
            verify,
            through: end,
        })
    }

    pub(crate) fn applied(&mut self, plan: &Plan) {
        if self.open.is_empty() {
            return;
        }
        let done = self.open.remove(0);
        for (pos, u) in done.uses.iter().enumerate() {
            if u.tool.name.starts_with(&self.relay_prefix) {
                self.saved.push(Pending {
                    turn: done.turn,
                    pos: pos as u32,
                    tool_use: u.tool.clone(),
                });
            }
            self.known.insert(
                u.tool.id.clone(),
                Known {
                    turn: done.turn,
                    pos: pos as u32,
                    name: u.tool.name.clone(),
                    input: u.tool.input.clone(),
                },
            );
        }
        self.buffer.retain(|l| l.seq >= plan.through);
        self.buffered = self.buffer.iter().map(|l| l.raw.len()).sum();
    }
}

fn step(key: String, body: Value) -> Step {
    Step {
        key,
        body: JsonPayload(body.to_string()),
    }
}
