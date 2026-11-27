//! Per-attempt broker queue matching AGY `/mcp` calls against broker-observed Gemini `call_mcp_tool` plans.

use super::observe::{Observed, ObservedUse};
use crate::{canon::canonicalize, json::parse};
use bot_core::{cli::GROUP_PREFIX, id::Attempt, ledger::CallLedger, port::Error};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

const MAX_TURN_PROBE: u64 = 4096;
const RELAY_PREFIX: &str = "mcp__relay__";

pub(super) type AttemptKey = (String, String, u64, Option<u64>);

fn key_of(attempt: &Attempt) -> AttemptKey {
    (
        attempt.profile.0.clone(),
        attempt.run.0.clone(),
        attempt.generation.0,
        attempt.claim.map(|c| c.0),
    )
}

struct PendingSlot {
    id: String,
    mcp_name: String,
    canonical_args: String,
}

struct BoundSlot {
    id: String,
    mcp_name: String,
    canonical_args: String,
}

#[derive(Default)]
struct AttemptQueue {
    next_turn: Option<u64>,
    /// Unbound slots from the most recently committed Gemini turn only (prevents cross-turn slot stealing).
    pending: Vec<PendingSlot>,
    /// Bound `_meta.progressToken` mappings retained for idempotent retries within the attempt.
    bound: HashMap<String, BoundSlot>,
}

#[derive(Default)]
pub(super) struct AgyCalls {
    queues: Mutex<HashMap<AttemptKey, AttemptQueue>>,
    /// Per-attempt async locks serializing Gemini turn probing and direct checkpoint commits.
    publish_locks: Mutex<HashMap<AttemptKey, Arc<tokio::sync::Mutex<()>>>>,
}

impl AgyCalls {
    /// Returns the per-attempt publish lock so concurrent Gemini streams cannot race on `next_turn`.
    pub(super) fn publish_lock(&self, attempt: &Attempt) -> Arc<tokio::sync::Mutex<()>> {
        let key = key_of(attempt);
        self.publish_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Returns the next unused `cli-group-<turn>` index for this attempt by probing the ledger once.
    /// Does not cache the probed value until `enqueue` commits so failed/timed-out publishes always re-probe.
    pub(super) async fn next_turn(
        &self,
        attempt: &Attempt,
        ledger: &dyn CallLedger,
    ) -> Result<u64, Error> {
        let key = key_of(attempt);
        if let Some(turn) = self
            .queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .and_then(|q| q.next_turn)
        {
            return Ok(turn);
        }
        // Probe persisted turn groups so fresh attempts after crash/resume never collide with earlier turns.
        for turn in 0..MAX_TURN_PROBE {
            let existing = ledger
                .step(attempt.clone(), format!("{GROUP_PREFIX}{turn}"))
                .await?;
            if existing.is_none() {
                return Ok(turn);
            }
        }
        Err(Error::Unavailable)
    }

    /// Invalidates cached turn cursor after an ambiguous/failed publish so the next call re-probes the ledger.
    pub(super) fn invalidate_turn(&self, attempt: &Attempt) {
        let key = key_of(attempt);
        if let Some(queue) = self
            .queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&key)
        {
            queue.next_turn = None;
        }
    }

    /// Rewrites an observed Gemini turn with deterministic `agy-<turn>` / `agy-<turn>-<pos>` IDs.
    pub(super) fn stamp(turn: u64, observed: Observed) -> Observed {
        let uses = observed
            .uses
            .into_iter()
            .enumerate()
            .map(|(pos, u)| ObservedUse {
                id: format!("agy-{turn}-{pos}"),
                name: u.name,
                input: u.input,
            })
            .collect();
        Observed {
            id: format!("agy-{turn}"),
            uses,
        }
    }

    /// Registers a committed turn's relay tool calls for FIFO matching on `/mcp`, expiring any prior-turn unbound slots.
    pub(super) fn enqueue(&self, attempt: &Attempt, turn: u64, stamped: &Observed) {
        let key = key_of(attempt);
        let mut map = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        let queue = map.entry(key).or_default();
        queue.next_turn = Some(turn.saturating_add(1));
        queue.pending.clear();
        for u in &stamped.uses {
            let Some(mcp_name) = u.name.strip_prefix(RELAY_PREFIX) else {
                continue;
            };
            let canonical_args = parse(&u.input)
                .and_then(|v| canonicalize(&v))
                .unwrap_or_else(|| u.input.clone());
            queue.pending.push(PendingSlot {
                id: u.id.clone(),
                mcp_name: mcp_name.to_owned(),
                canonical_args,
            });
        }
    }

    /// Matches an incoming AGY `/mcp` call by `_meta.progressToken` (idempotent retry) or next FIFO slot in the current turn.
    pub(super) fn resolve(
        &self,
        attempt: &Attempt,
        progress_token: &str,
        mcp_name: &str,
        canonical_args: &str,
    ) -> Result<String, Error> {
        let key = key_of(attempt);
        let mut map = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        let Some(queue) = map.get_mut(&key) else {
            return Err(Error::Conflict);
        };
        if let Some(slot) = queue.bound.get(progress_token) {
            return if slot.mcp_name == mcp_name && slot.canonical_args == canonical_args {
                Ok(slot.id.clone())
            } else {
                Err(Error::Conflict)
            };
        }
        let Some(idx) = queue
            .pending
            .iter()
            .position(|slot| slot.mcp_name == mcp_name && slot.canonical_args == canonical_args)
        else {
            return Err(Error::Conflict);
        };
        let slot = queue.pending.remove(idx);
        let id = slot.id.clone();
        queue.bound.insert(
            progress_token.to_owned(),
            BoundSlot {
                id: slot.id,
                mcp_name: slot.mcp_name,
                canonical_args: slot.canonical_args,
            },
        );
        Ok(id)
    }

    pub(super) fn clear(&self, attempt: &Attempt) {
        let key = key_of(attempt);
        self.queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        self.publish_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }

    /// Drops state for attempts that no longer have any active session in `SessionTable`.
    pub(super) fn prune(&self, active: &HashSet<AttemptKey>) {
        self.queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| active.contains(k));
        self.publish_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| active.contains(k));
    }
}
