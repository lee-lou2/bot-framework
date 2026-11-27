//! Pure Codex rollout checkpoint restoration, trusted ledger rewriting, and synthetic output synthesis.

use super::session::{
    CallParse, Known, function_call_output_id, parse_function_call, session_thread_id,
};
use crate::model::session::{Pending, ToolOutput, ToolUse};
use bot_core::{
    cli::{CliGroup, CliUse, GROUP_PREFIX, RECORD_PREFIX, USE_PREFIX},
    command::Step,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Corrupt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Saved {
    pub thread_id: String,
    pub lines: Vec<String>,
    pub last_group: Vec<(u64, u32, ToolUse)>,
    pub answered: BTreeSet<String>,
    pub known: BTreeMap<String, Known>,
}

fn format_tool_output(output: &ToolOutput) -> String {
    serde_json::to_string(output).unwrap_or_else(|_| r#"{"content":[],"isError":true}"#.to_owned())
}

impl Saved {
    pub(crate) fn pending(&self) -> Vec<Pending> {
        self.last_group
            .iter()
            .filter(|(_, _, u)| !self.answered.contains(&u.id))
            .map(|(turn, pos, tool_use)| Pending {
                turn: *turn,
                pos: *pos,
                tool_use: tool_use.clone(),
            })
            .collect()
    }

    pub(crate) fn next_seq(&self) -> u64 {
        self.lines.len() as u64
    }

    /// Returns the next monotonic Codex rollout `ordinal` if any saved record carries one.
    pub(crate) fn next_ordinal(&self) -> Option<u64> {
        self.lines
            .iter()
            .filter_map(|line| {
                serde_json::from_str::<Value>(line)
                    .ok()?
                    .get("ordinal")?
                    .as_u64()
            })
            .max()
            .map(|max| max.saturating_add(1))
    }

    pub(crate) fn relay_results(&self, prefix: &str) -> Vec<Pending> {
        self.answered
            .iter()
            .filter_map(|id| {
                let known = self.known.get(id)?;
                known.name.starts_with(prefix).then(|| Pending {
                    turn: known.turn,
                    pos: known.pos,
                    tool_use: ToolUse {
                        id: id.clone(),
                        name: known.name.clone(),
                        input: known.input.clone(),
                    },
                })
            })
            .collect()
    }

    /// Overwrites VM-written relay `function_call_output` payloads with trusted ledger outputs.
    pub(crate) fn trust(&mut self, trusted: &BTreeMap<String, ToolOutput>) -> Result<(), Corrupt> {
        for line in &mut self.lines {
            let mut value: Value = serde_json::from_str(line).map_err(|_| Corrupt)?;
            let Some(call_id) = function_call_output_id(&value).map(str::to_owned) else {
                continue;
            };
            if let Some(output) = trusted.get(&call_id) {
                let payload = value
                    .get_mut("payload")
                    .and_then(Value::as_object_mut)
                    .ok_or(Corrupt)?;
                payload.insert("output".into(), Value::String(format_tool_output(output)));
                *line = value.to_string();
            }
        }
        Ok(())
    }
}

pub(crate) fn restore(steps: &[Step]) -> Result<Option<Saved>, Corrupt> {
    let mut recs: BTreeMap<u64, String> = BTreeMap::new();
    let mut groups: BTreeMap<u64, CliGroup> = BTreeMap::new();
    let mut uses: BTreeMap<&str, CliUse> = BTreeMap::new();
    for step in steps {
        if let Some(seq) = step.key.strip_prefix(RECORD_PREFIX) {
            let line: Value = serde_json::from_str(&step.body.0).map_err(|_| Corrupt)?;
            recs.insert(
                seq.parse().map_err(|_| Corrupt)?,
                line.as_str().ok_or(Corrupt)?.to_owned(),
            );
        } else if let Some(turn) = step.key.strip_prefix(GROUP_PREFIX) {
            let turn: u64 = turn.parse().map_err(|_| Corrupt)?;
            let group: CliGroup = serde_json::from_str(&step.body.0).map_err(|_| Corrupt)?;
            if group.seq != turn || group.uses.is_empty() {
                return Err(Corrupt);
            }
            groups.insert(turn, group);
        } else if let Some(id) = step.key.strip_prefix(USE_PREFIX) {
            uses.insert(
                id,
                serde_json::from_str::<CliUse>(&step.body.0).map_err(|_| Corrupt)?,
            );
        }
    }
    if groups.is_empty() {
        return Ok(None);
    }
    if recs.keys().copied().ne(0..recs.len() as u64) {
        return Err(Corrupt);
    }
    let lines: Vec<String> = recs.into_values().collect();
    let values: Vec<Value> = lines
        .iter()
        .map(|l| serde_json::from_str(l).map_err(|_| Corrupt))
        .collect::<Result<_, _>>()?;
    let thread_id = values
        .first()
        .and_then(session_thread_id)
        .ok_or(Corrupt)?
        .to_owned();
    let mut answered = BTreeSet::new();
    for value in &values {
        if let Some(id) = function_call_output_id(value) {
            answered.insert(id.to_owned());
        }
    }
    let mut known = BTreeMap::new();
    let mut last_group = Vec::new();
    let newest = *groups.keys().next_back().ok_or(Corrupt)?;
    for (turn, group) in &groups {
        let mut slots = Vec::new();
        for (pos, id) in group.uses.iter().enumerate() {
            let plan = uses.get(id.as_str()).ok_or(Corrupt)?;
            let line = values.get(plan.seq as usize).ok_or(Corrupt)?;
            let CallParse::Valid(in_line) = parse_function_call(line) else {
                return Err(Corrupt);
            };
            if in_line.id != *id
                || plan.turn != *turn
                || plan.pos as usize != pos
                || in_line.name != plan.name
                || in_line.input != plan.input
                || *turn > plan.seq
                || (pos == 0 && plan.seq != *turn)
            {
                return Err(Corrupt);
            }
            known.insert(
                id.clone(),
                Known {
                    turn: *turn,
                    pos: pos as u32,
                    name: in_line.name.clone(),
                    input: in_line.input.clone(),
                },
            );
            slots.push((*turn, pos as u32, in_line));
        }
        if *turn == newest {
            last_group = slots;
        } else if slots.iter().any(|(_, _, u)| !answered.contains(&u.id)) {
            return Err(Corrupt);
        }
    }
    Ok(Some(Saved {
        thread_id,
        lines,
        last_group,
        answered,
        known,
    }))
}

pub(crate) fn output_line(
    call_id: &str,
    output: &ToolOutput,
    now: &str,
    ordinal: Option<u64>,
) -> String {
    let mut record = json!({
        "timestamp": now,
        "type": "response_item",
        "payload": {
            "type": "function_call_output",
            "call_id": call_id,
            "output": format_tool_output(output),
        }
    });
    if let Some(ord) = ordinal
        && let Some(obj) = record.as_object_mut()
    {
        obj.insert("ordinal".into(), Value::from(ord));
    }
    record.to_string()
}

pub(crate) fn restored_rollout_path(thread_id: &str) -> String {
    format!("codex/sessions/2026/01/01/rollout-2026-01-01T00-00-00-{thread_id}.jsonl")
}
