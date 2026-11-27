//! Durable Claude Code JSONL session persistence and restoration protocol across ephemeral VM claims.
//!
//! Only conversation-chain lines (`user`/`assistant`/`attachment` with `uuid`) are checkpointed in
//! atomic batches (`cli-rec-*`, `cli-use-*`, `cli-group-*`) once they match the front's `cli-resp-*`
//! confirmation; relay tool results are never trusted from the VM and are overwritten from the ledger.

mod journal;
mod open;
mod restore;
mod tail;

pub use bot_core::cli::ToolOutput;
pub use journal::{Fed, Journal, Plan, Refused, Why};
pub use restore::{Corrupt, Saved, restore, result_line};
pub use tail::Tail;

use bot_core::{
    cli::{CliResponse, RECORD_PREFIX},
    command::{JsonPayload, Step},
    id::{ProfileId, RunId},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fmt, future::Future, pin::Pin, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionError {
    Lost,
    Unavailable,
    Permanent(&'static str),
}

pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SessionError>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pending {
    pub turn: u64,
    pub pos: u32,
    pub tool_use: ToolUse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Slot {
    Result(ToolOutput),
    Parked,
}

/// Host port supplying checkpoint I/O, front response confirmation, trusted ledger recall, and slot settlement.
pub trait SessionPort: Send + Sync {
    fn steps(&self) -> &[Step];
    fn commit(&self, steps: Vec<Step>) -> PortFuture<'_, ()>;
    fn response(&self, message_id: &str) -> PortFuture<'_, Option<CliResponse>>;
    /// Re-reads a recorded relay slot from the ledger without re-executing so restored sessions overwrite VM-written results.
    fn recall<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Option<ToolOutput>>;
    /// Reports whether the slot reached a terminal ledger state (completed or confirmed refusal) without executing.
    fn ended<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, bool>;
    /// Settles an unanswered slot under current authorization/approval state before resuming the CLI.
    fn settle<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Slot>;
}

#[derive(Clone)]
pub struct Session(pub Arc<dyn SessionPort>);

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Session")
    }
}

/// Below the 1 MiB core checkpoint limit to leave JSON framing headroom.
pub const MAX_LINE_BYTES: usize = 900 * 1024;

pub fn rec_key(seq: u64) -> String {
    format!("{RECORD_PREFIX}{seq}")
}

pub fn rec_step(seq: u64, line: &str) -> Step {
    Step {
        key: rec_key(seq),
        body: JsonPayload(Value::String(line.to_owned()).to_string()),
    }
}

fn uuid_from(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

pub fn session_id(profile: &ProfileId, run: &RunId) -> String {
    uuid_from(&format!("bot-cli-session|{}|{}", profile.0, run.0))
}

/// Maps working directory to Claude Code's project folder name (`/bot` -> `-bot`).
pub fn project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

fn blocks(v: &Value) -> impl Iterator<Item = &Value> {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn kind(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or_default()
}

pub fn is_chain(v: &Value) -> bool {
    matches!(kind(v), "user" | "assistant" | "attachment")
        && v.get("uuid").and_then(Value::as_str).is_some()
}

pub fn uuid_of(v: &Value) -> Option<&str> {
    v.get("uuid").and_then(Value::as_str)
}

pub fn message_id(v: &Value) -> Option<&str> {
    (kind(v) == "assistant")
        .then(|| v.get("message")?.get("id")?.as_str())
        .flatten()
}

pub fn tool_uses(v: &Value) -> Vec<ToolUse> {
    if kind(v) != "assistant" {
        return Vec::new();
    }
    blocks(v)
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|b| {
            Some(ToolUse {
                id: b.get("id")?.as_str()?.to_owned(),
                name: b.get("name")?.as_str()?.to_owned(),
                input: b.get("input").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

pub fn tool_results(v: &Value) -> Vec<String> {
    if kind(v) != "user" {
        return Vec::new();
    }
    blocks(v)
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| b.get("tool_use_id")?.as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests;
