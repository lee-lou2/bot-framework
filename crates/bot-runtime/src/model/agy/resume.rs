//! Broker-ledger session recall and settlement for AGY ephemeral resume across approvals and crashes.

use crate::model::{
    CallEnd, DoneCall, Exchange, ModelError, ToolOutput,
    cli::{self, session_error},
    session::{Pending, Session, Slot, ToolUse},
};
use bot_core::{
    builtin::DELEGATE_TOOL,
    canon::canonicalize,
    cli::{CliGroup, CliUse, GROUP_PREFIX, USE_PREFIX},
    command::JsonPayload,
    id::{CallId, ToolId},
};
use serde_json::Value;
use std::collections::BTreeMap;

/// Recalls and settles all broker-recorded AGY tool turns into trusted `Exchange`s before relaunching ephemerally.
pub(crate) async fn prepare(
    session: &Session,
    relay_prefix: &str,
) -> Result<Option<Vec<Exchange>>, ModelError> {
    let port = &session.0;
    let mut groups: BTreeMap<u64, CliGroup> = BTreeMap::new();
    let mut uses: BTreeMap<&str, CliUse> = BTreeMap::new();
    for step in port.steps() {
        if let Some(turn) = step.key.strip_prefix(GROUP_PREFIX) {
            let turn: u64 = turn.parse().map_err(|_| cli::failed("session_corrupt"))?;
            let group: CliGroup =
                serde_json::from_str(&step.body.0).map_err(|_| cli::failed("session_corrupt"))?;
            if group.seq != turn || group.uses.is_empty() {
                return Err(cli::failed("session_corrupt"));
            }
            groups.insert(turn, group);
        } else if let Some(id) = step.key.strip_prefix(USE_PREFIX) {
            let entry: CliUse =
                serde_json::from_str(&step.body.0).map_err(|_| cli::failed("session_corrupt"))?;
            uses.insert(id, entry);
        }
    }
    if groups.is_empty() {
        return Ok(None);
    }
    let mut exchanges = Vec::with_capacity(groups.len());
    for (turn, group) in groups {
        let mut calls = Vec::with_capacity(group.uses.len());
        for (pos, id) in group.uses.iter().enumerate() {
            let plan = uses
                .get(id.as_str())
                .ok_or_else(|| cli::failed("session_corrupt"))?;
            if plan.turn != turn || plan.pos as usize != pos || !plan.input.is_object() {
                return Err(cli::failed("session_corrupt"));
            }
            let tool =
                map_tool(&plan.name, relay_prefix).ok_or_else(|| cli::failed("session_corrupt"))?;
            let slot = Pending {
                turn,
                pos: pos as u32,
                tool_use: ToolUse {
                    id: id.clone(),
                    name: plan.name.clone(),
                    input: plan.input.clone(),
                },
            };
            let output = match port.recall(&slot).await.map_err(session_error)? {
                Some(output) => output,
                None => match port.settle(&slot).await.map_err(session_error)? {
                    Slot::Result(output) => output,
                    Slot::Parked => return Err(ModelError::Parked),
                },
            };
            let args =
                JsonPayload(canonicalize(&plan.input).unwrap_or_else(|| plan.input.to_string()));
            calls.push(DoneCall {
                call: CallId::cli(turn, pos as u32),
                tool,
                args,
                end: output_to_end(output),
            });
        }
        exchanges.push(Exchange {
            text: String::new(),
            calls,
            state: None,
        });
    }
    Ok(Some(exchanges))
}

fn map_tool(full_name: &str, relay_prefix: &str) -> Option<ToolId> {
    let mcp_name = full_name.strip_prefix(relay_prefix)?;
    if mcp_name.is_empty() {
        return None;
    }
    if mcp_name == DELEGATE_TOOL {
        Some(ToolId(DELEGATE_TOOL.to_owned()))
    } else {
        // Preserve hallucinated relay tool names so resume settles them cleanly instead of failing as corrupt.
        Some(ToolId::from_mcp_name(mcp_name).unwrap_or_else(|| ToolId(mcp_name.to_owned())))
    }
}

fn output_to_end(output: ToolOutput) -> CallEnd {
    if !output.is_error {
        let payload = match output.content.as_slice() {
            [one]
                if one.get("type").and_then(Value::as_str) == Some("text")
                    && one.get("text").and_then(Value::as_str).is_some() =>
            {
                one["text"].as_str().unwrap_or_default().to_owned()
            }
            _ => serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_owned()),
        };
        return CallEnd::Done(JsonPayload(payload));
    }
    let word = output
        .content
        .first()
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("failed");
    if word == "denied" {
        CallEnd::Denied
    } else {
        CallEnd::Failed(word.to_owned())
    }
}
