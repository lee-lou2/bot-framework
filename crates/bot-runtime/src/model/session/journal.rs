//! Pure state machine turning tailed Claude Code JSONL lines into front-verified checkpoint batches.

use super::{
    MAX_LINE_BYTES, Pending, ToolUse, is_chain, message_id, rec_step, tool_results, tool_uses,
};
use bot_core::{
    cli::{CliGroup, CliResponse, CliUse, GROUP_PREFIX, USE_PREFIX},
    command::{JsonPayload, Step},
};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fed {
    Skipped,
    Accepted,
    Frozen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Why {
    /// Unfinished placeholder result or similar transient freeze; resume can fill the pending slot.
    Unsaved,
    /// Line or unconfirmed buffer exceeded size/count caps; retrying would hit the same limit.
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refused {
    NotYet,
    Incomplete,
    Mismatch,
    Unverified,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Known {
    pub turn: u64,
    pub pos: u32,
    pub name: String,
    pub input: Value,
}

const MAX_BUFFERED_BYTES: usize = 16 << 20;
const MAX_BUFFERED_LINES: usize = 4096;

#[derive(Debug)]
pub struct Plan {
    /// Ordered as records -> `cli-use` -> `cli-group` (last).
    pub steps: Vec<Step>,
    /// Relay tool calls whose results appear in this batch and must be confirmed finished in the ledger before commit.
    pub verify: Vec<Pending>,
    through: u64,
}

pub struct Journal {
    next: u64,
    buffer: Vec<Line>,
    open: Vec<Open>,
    buffered: usize,
    known: BTreeMap<String, Known>,
    relay_prefix: String,
    frozen: Option<Why>,
    /// Tracks whether any relay tool call was dropped while frozen so the run cannot report success.
    blocked: bool,
    saved: Vec<Pending>,
}

impl Journal {
    pub fn new(next: u64, known: BTreeMap<String, Known>, relay_prefix: String) -> Self {
        Self {
            next,
            buffer: Vec::new(),
            open: Vec::new(),
            buffered: 0,
            known,
            relay_prefix,
            frozen: None,
            blocked: false,
            saved: Vec::new(),
        }
    }

    pub fn frozen(&self) -> bool {
        self.frozen.is_some()
    }

    pub fn stuck(&self) -> Option<Why> {
        self.frozen.filter(|_| self.blocked)
    }

    pub fn saved_relay(&self) -> &[Pending] {
        &self.saved
    }

    pub fn unsaved_relay(&self) -> bool {
        self.open
            .iter()
            .flat_map(|o| &o.uses)
            .any(|u| u.tool.name.starts_with(&self.relay_prefix))
    }

    pub fn freeze(&mut self, why: Why) {
        self.blocked |= self.unsaved_relay();
        self.frozen.get_or_insert(why);
        self.buffer.clear();
        self.open.clear();
        self.buffered = 0;
    }

    pub fn oldest(&self) -> Option<&str> {
        self.open.first().map(|o| o.id.as_str())
    }

    pub fn feed(&mut self, raw: &str) -> Fed {
        if self.frozen.is_some() {
            // Even after freezing, detect any later relay tool_use so the attempt fails instead of succeeding silently.
            if !self.blocked && raw.contains("\"tool_use\"") {
                self.blocked = serde_json::from_str::<Value>(raw).is_ok_and(|v| {
                    tool_uses(&v)
                        .iter()
                        .any(|u| u.name.starts_with(&self.relay_prefix))
                });
            }
            return Fed::Skipped;
        }
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return Fed::Skipped;
        };
        if !is_chain(&value) {
            return Fed::Skipped;
        }
        if raw.len() > MAX_LINE_BYTES
            || self.buffered + raw.len() > MAX_BUFFERED_BYTES
            || self.buffer.len() >= MAX_BUFFERED_LINES
        {
            self.freeze(Why::TooLarge);
            self.blocked |= tool_uses(&value)
                .iter()
                .any(|u| u.name.starts_with(&self.relay_prefix));
            return Fed::Frozen;
        }
        self.buffered += raw.len();
        let seq = self.next;
        self.next += 1;
        let uses = tool_uses(&value);
        if !uses.is_empty()
            && let Some(id) = message_id(&value)
        {
            let at = match self.open.iter().position(|o| o.id == id) {
                Some(at) => at,
                None => {
                    // Anchor the turn at the first assistant line for this message.id (which may be text-only).
                    let turn = self.first_line_of(id).unwrap_or(seq);
                    self.open.push(Open {
                        id: id.to_owned(),
                        turn,
                        uses: Vec::new(),
                    });
                    self.open.len() - 1
                }
            };
            self.open[at]
                .uses
                .extend(uses.into_iter().map(|tool| Located { seq, tool }));
        }
        self.buffer.push(Line {
            seq,
            raw: raw.to_owned(),
            value,
        });
        Fed::Accepted
    }

    fn first_line_of(&self, id: &str) -> Option<u64> {
        self.buffer
            .iter()
            .find(|l| message_id(&l.value) == Some(id))
            .map(|l| l.seq)
    }

    /// Plans a batch for the oldest open response once its tool uses match the front's confirmation exactly.
    pub fn plan(&self, response: &CliResponse) -> Result<Plan, Refused> {
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
            for id in tool_results(&line.value) {
                let place = self.known.get(&id).map(|k| Pending {
                    turn: k.turn,
                    pos: k.pos,
                    tool_use: ToolUse {
                        id: id.clone(),
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

    pub fn applied(&mut self, plan: &Plan) {
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
