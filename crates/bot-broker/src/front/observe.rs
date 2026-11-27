//! Strict observation of completed model responses to persist ordered `tool_use` plans.
//!
//! The front withholds the terminal `message_stop` event or JSON body until `TurnSink::completed`
//! succeeds. Any ambiguous or malformed stream framing rejects the whole response so the recorded
//! plan cannot diverge from what the CLI executes.

use crate::json::parse;
use bot_core::{
    cli::{CliRefusal, CliResponse, CliResponseUse, RESPONSE_PREFIX},
    command::{JsonPayload, Step},
    id::{Attempt, CallId},
    port::{Error, Reply, RunStore},
};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedUse {
    pub id: String,
    pub name: String,
    pub input: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observed {
    pub id: String,
    pub uses: Vec<ObservedUse>,
}

/// Sink for observed turn plans and deterministic pre-execution CLI refusal markers.
pub trait TurnSink: Send + Sync {
    fn completed(&self, attempt: Attempt, turn: Observed) -> Reply<'_, ()>;

    fn refused(&self, _: Attempt, _: CallId, _: CliRefusal) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

pub struct NoSink;

impl TurnSink for NoSink {
    fn completed(&self, _: Attempt, _: Observed) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// Persists observed responses (`cli-resp-<id>`) and refusal markers into run checkpoints.
pub struct CheckpointSink {
    runs: Arc<dyn RunStore>,
}

impl CheckpointSink {
    pub fn new(runs: Arc<dyn RunStore>) -> Self {
        Self { runs }
    }
}

/// Slightly under the 1 MiB store checkpoint step cap.
const MAX_STEP: usize = 900 << 10;

impl TurnSink for CheckpointSink {
    fn completed(&self, attempt: Attempt, turn: Observed) -> Reply<'_, ()> {
        Box::pin(async move {
            let mut uses = Vec::new();
            for u in &turn.uses {
                let input: Value = serde_json::from_str(&u.input).map_err(|_| Error::Invalid)?;
                uses.push(CliResponseUse {
                    id: u.id.clone(),
                    name: u.name.clone(),
                    input,
                });
            }
            let response = CliResponse { id: turn.id, uses };
            if !response.is_valid() {
                return Err(Error::Invalid);
            }
            let body = serde_json::to_string(&response).map_err(|_| Error::Invalid)?;
            if body.len() > MAX_STEP {
                return Err(Error::Invalid);
            }
            let step = Step {
                key: format!("{RESPONSE_PREFIX}{}", response.id),
                body: JsonPayload(body),
            };
            self.runs.checkpoint(attempt, step).await
        })
    }

    fn refused(&self, attempt: Attempt, call: CallId, refusal: CliRefusal) -> Reply<'_, ()> {
        self.runs.checkpoint(attempt, refusal.step(&call))
    }
}

fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn observed(message: Option<String>, uses: Vec<ObservedUse>) -> Result<Option<Observed>, Error> {
    if uses.is_empty() {
        return Ok(None);
    }
    let id = message.filter(|id| id_ok(id)).ok_or(Error::Invalid)?;
    let mut seen = std::collections::BTreeSet::new();
    for u in &uses {
        if !id_ok(&u.id) || u.name.is_empty() || !seen.insert(u.id.as_str()) {
            return Err(Error::Invalid);
        }
    }
    Ok(Some(Observed { id, uses }))
}

struct Open {
    id: String,
    name: String,
    json: String,
    /// Non-empty JSON object supplied directly in `content_block_start`.
    initial: Option<Value>,
}

enum Block {
    Other,
    Tool(Open),
    Closed(Option<ObservedUse>),
}

const MAX_INPUT: usize = 900 << 10;

/// Events whose SSE `event:` header must strictly match `data.type`.
const TRACKED: [&str; 6] = [
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

/// Incremental SSE observer that marks `broken` on any framing ambiguity (duplicate keys or indices,
/// mismatched event names, stray deltas, non-object inputs, or unclosed blocks).
#[derive(Default)]
pub(super) struct Observer {
    message: Option<String>,
    started: bool,
    blocks: BTreeMap<u64, Block>,
    stopped: bool,
    broken: bool,
}

impl Observer {
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
        self.event(name, &value);
    }

    fn event(&mut self, name: Option<&str>, value: &Value) {
        let kind = value.get("type").and_then(Value::as_str);
        let tracked = name.is_some_and(|n| TRACKED.contains(&n))
            || kind.is_some_and(|k| TRACKED.contains(&k));
        if tracked && name != kind {
            self.broken = true;
            return;
        }
        let index = value.get("index").and_then(Value::as_u64);
        match kind {
            Some("message_start") => {
                if self.started {
                    self.broken = true;
                    return;
                }
                self.started = true;
                self.message = value
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("content_block_start") => self.start(index, value.get("content_block")),
            Some("content_block_delta") => self.delta(index, value.get("delta")),
            Some("content_block_stop") => self.stop(index),
            Some("message_stop") => self.stopped = true,
            _ => {}
        }
    }

    fn start(&mut self, index: Option<u64>, block: Option<&Value>) {
        let (Some(index), Some(block)) = (index, block) else {
            self.broken = true;
            return;
        };
        if self.blocks.contains_key(&index) {
            self.broken = true;
            return;
        }
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            self.blocks.insert(index, Block::Other);
            return;
        }
        let (Some(id), Some(name)) = (
            block.get("id").and_then(Value::as_str),
            block.get("name").and_then(Value::as_str),
        ) else {
            self.broken = true;
            return;
        };
        // Reject non-object `input` values rather than silently coercing them to `{}`.
        let initial = match block.get("input") {
            None => None,
            Some(Value::Object(map)) if map.is_empty() => None,
            Some(input @ Value::Object(_)) => Some(input.clone()),
            Some(_) => {
                self.broken = true;
                return;
            }
        };
        self.blocks.insert(
            index,
            Block::Tool(Open {
                id: id.to_owned(),
                name: name.to_owned(),
                json: String::new(),
                initial,
            }),
        );
    }

    fn delta(&mut self, index: Option<u64>, delta: Option<&Value>) {
        let (Some(index), Some(delta)) = (index, delta) else {
            self.broken = true;
            return;
        };
        let json_delta = delta.get("type").and_then(Value::as_str) == Some("input_json_delta");
        match self.blocks.get_mut(&index) {
            None | Some(Block::Closed(_)) => self.broken = true,
            Some(Block::Other) => self.broken |= json_delta,
            Some(Block::Tool(open)) => {
                let part = delta.get("partial_json").and_then(Value::as_str);
                match part {
                    Some(part) if json_delta && open.json.len() + part.len() <= MAX_INPUT => {
                        open.json.push_str(part);
                    }
                    _ => self.broken = true,
                }
            }
        }
    }

    fn stop(&mut self, index: Option<u64>) {
        let Some(index) = index else {
            self.broken = true;
            return;
        };
        let closed = match self.blocks.remove(&index) {
            None | Some(Block::Closed(_)) => {
                self.broken = true;
                return;
            }
            Some(Block::Other) => None,
            Some(Block::Tool(open)) => match finish_input(&open) {
                Some(input) => Some(ObservedUse {
                    id: open.id,
                    name: open.name,
                    input,
                }),
                None => {
                    self.broken = true;
                    return;
                }
            },
        };
        self.blocks.insert(index, Block::Closed(closed));
    }

    pub(super) fn stopped(&self) -> bool {
        self.stopped
    }

    pub(super) fn broken(&self) -> bool {
        self.broken
    }

    pub(super) fn finish(&mut self) -> Result<Option<Observed>, Error> {
        let unfinished = self
            .blocks
            .values()
            .any(|block| !matches!(block, Block::Closed(_)));
        if self.broken || unfinished || !self.started {
            return Err(Error::Invalid);
        }
        let uses: Vec<ObservedUse> = std::mem::take(&mut self.blocks)
            .into_values()
            .filter_map(|block| match block {
                Block::Closed(Some(used)) => Some(used),
                _ => None,
            })
            .collect();
        observed(self.message.take(), uses)
    }
}

/// Rejects ambiguous tool inputs that supply both a non-empty start object and JSON deltas.
fn finish_input(open: &Open) -> Option<String> {
    let fragments = !open.json.trim().is_empty();
    match (&open.initial, fragments) {
        (Some(_), true) => None,
        (Some(initial), false) => Some(initial.to_string()),
        (None, false) => Some("{}".to_owned()),
        (None, true) => match parse(&open.json)? {
            value @ Value::Object(_) => Some(value.to_string()),
            _ => None,
        },
    }
}

pub(super) fn observe_body(body: &str) -> Result<Option<Observed>, Error> {
    let Some(value) = parse(body) else {
        return Err(Error::Invalid);
    };
    let Some(blocks) = value.get("content").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut uses = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let (Some(id), Some(name), Some(input)) = (
            block.get("id").and_then(Value::as_str),
            block.get("name").and_then(Value::as_str),
            block.get("input").filter(|v| v.is_object()),
        ) else {
            return Err(Error::Invalid);
        };
        uses.push(ObservedUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input: input.to_string(),
        });
    }
    let message = value.get("id").and_then(Value::as_str).map(str::to_owned);
    observed(message, uses)
}
