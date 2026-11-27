//! Resolves CLI `tools/call` requests against checkpointed turn plans to assign logical `CallId`s.
//!
//! Reads `cli-use-<tool_use_id>` and `cli-group-<turn>` checkpoints written atomically per model turn.
//! Verifies the entire turn group (complete uses, matching turn/pos, no duplicate IDs) and exact relay-prefixed
//! tool name plus canonical arguments within one absolute deadline before authorizing a `CallId::cli(turn, pos)`.

use super::CallBook;
use crate::{canon::canonicalize, json::parse, service::preflight::Preflight};
use bot_core::{
    cli::{CliGroup, CliRefusal, CliUse, GROUP_PREFIX, USE_PREFIX},
    command::{JsonPayload, ToolCall},
    id::{Attempt, CallId, ToolId},
    ledger::CallLedger,
    port::{Error, Reply},
};
use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::{Instant, sleep, timeout_at};

/// `(profile, run, turn)`.
type Group = (String, String, u64);

const DEFAULT_CACHE: usize = 1024;

/// Bounded FIFO cache of verified immutable turn groups; eviction only triggers re-verification.
struct Verified {
    limit: usize,
    order: VecDeque<Group>,
    seen: HashSet<Group>,
}

impl Verified {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            order: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

    fn contains(&self, group: &Group) -> bool {
        self.seen.contains(group)
    }

    fn insert(&mut self, group: Group) {
        if self.limit == 0 {
            return;
        }
        if self.seen.insert(group.clone()) {
            self.order.push_back(group);
            while self.order.len() > self.limit {
                if let Some(oldest) = self.order.pop_front() {
                    self.seen.remove(&oldest);
                }
            }
        }
    }
}

pub struct PlannedCalls {
    ledger: Arc<dyn CallLedger>,
    wait: Duration,
    poll: Duration,
    /// Relay MCP server alias used in provider-visible `mcp__<server>__<tool>` names.
    server: String,
    verified: Mutex<Verified>,
    /// Optional preflight oracle for confirming `UnknownTool`/`Invalid` refusals before marking.
    preflight: Option<Arc<dyn Preflight>>,
}

impl PlannedCalls {
    /// `wait` is the single absolute deadline covering both polling and ledger reads (100ms poll interval).
    pub fn new(ledger: Arc<dyn CallLedger>, wait: Duration) -> Self {
        Self {
            ledger,
            wait,
            poll: Duration::from_millis(100),
            server: "relay".to_owned(),
            verified: Mutex::new(Verified::new(DEFAULT_CACHE)),
            preflight: None,
        }
    }

    pub fn with_preflight(mut self, preflight: Arc<dyn Preflight>) -> Self {
        self.preflight = Some(preflight);
        self
    }

    /// Sets the verified-group cache cap (default 1024; 0 disables caching).
    pub fn with_cache_limit(mut self, limit: usize) -> Self {
        self.verified = Mutex::new(Verified::new(limit));
        self
    }

    #[cfg(test)]
    pub(crate) fn cached_groups(&self) -> usize {
        self.verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seen
            .len()
    }

    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    async fn step(
        &self,
        attempt: &Attempt,
        key: String,
        deadline: Instant,
    ) -> Result<Option<JsonPayload>, Error> {
        timeout_at(deadline, self.ledger.step(attempt.clone(), key))
            .await
            .map_err(|_| Error::Unavailable)?
    }

    async fn read(
        &self,
        attempt: &Attempt,
        id: &str,
        deadline: Instant,
    ) -> Result<Option<CliUse>, Error> {
        let Some(body) = self
            .step(attempt, format!("{USE_PREFIX}{id}"), deadline)
            .await?
        else {
            return Ok(None);
        };
        let plan = use_of(&body)?;
        // Group and uses commit together; a missing or inconsistent group is corrupt.
        let body = self
            .step(attempt, format!("{GROUP_PREFIX}{}", plan.turn), deadline)
            .await?
            .ok_or(Error::Invalid)?;
        let value = parse(&body.0).ok_or(Error::Invalid)?;
        let group: CliGroup = serde_json::from_value(value).map_err(|_| Error::Invalid)?;
        if group.seq != plan.turn
            || group.uses.get(plan.pos as usize).map(String::as_str) != Some(id)
            || plan.seq < plan.turn
        {
            return Err(Error::Invalid);
        }
        let key: Group = (attempt.profile.0.clone(), attempt.run.0.clone(), group.seq);
        if !self
            .verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key)
        {
            self.verify_group(attempt, &group, deadline).await?;
            self.verified
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key);
        }
        Ok(Some(plan))
    }

    async fn verify_group(
        &self,
        attempt: &Attempt,
        group: &CliGroup,
        deadline: Instant,
    ) -> Result<(), Error> {
        let mut seen = HashSet::new();
        for (pos, id) in group.uses.iter().enumerate() {
            if !seen.insert(id.as_str()) {
                return Err(Error::Invalid);
            }
            let body = self
                .step(attempt, format!("{USE_PREFIX}{id}"), deadline)
                .await?
                .ok_or(Error::Invalid)?;
            let plan = use_of(&body)?;
            if plan.turn != group.seq || plan.pos as usize != pos || plan.seq < plan.turn {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
}

fn use_of(body: &JsonPayload) -> Result<CliUse, Error> {
    let value = parse(&body.0).ok_or(Error::Invalid)?;
    serde_json::from_value(value).map_err(|_| Error::Invalid)
}

impl CallBook for PlannedCalls {
    fn call_id(
        &self,
        attempt: Attempt,
        tool_use: String,
        name: String,
        args: JsonPayload,
    ) -> Reply<'_, CallId> {
        Box::pin(async move {
            let deadline = Instant::now() + self.wait;
            loop {
                if let Some(plan) = self.read(&attempt, &tool_use, deadline).await? {
                    let same_args = canonicalize(&plan.input).as_deref() == Some(args.0.as_str());
                    // Require the exact relay prefix so another server or builtin cannot alias this call.
                    let planned = format!("mcp__{}__{name}", self.server);
                    return if plan.name == planned && same_args {
                        Ok(CallId::cli(plan.turn, plan.pos))
                    } else {
                        Err(Error::Conflict)
                    };
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(Error::Unavailable);
                }
                sleep(self.poll.min(deadline - now)).await;
            }
        })
    }

    /// Replays a refusal marker only if the call ledger does not already hold a stronger state.
    fn marked(&self, attempt: Attempt, call: CallId) -> Reply<'_, Option<CliRefusal>> {
        Box::pin(async move {
            let deadline = Instant::now() + self.wait;
            let Some(stored) = self
                .step(&attempt, CliRefusal::key(&call), deadline)
                .await?
            else {
                return Ok(None);
            };
            let known = timeout_at(deadline, self.ledger.lookup(attempt, call))
                .await
                .map_err(|_| Error::Unavailable)??;
            if !CliRefusal::can_replay(known.as_ref()) {
                return Ok(None);
            }
            CliRefusal::decode(&stored)
                .map(Some)
                .map_err(|_| Error::Unavailable)
        })
    }

    fn confirm(
        &self,
        attempt: Attempt,
        call: CallId,
        tool: ToolId,
        args: JsonPayload,
        expected: CliRefusal,
    ) -> Reply<'_, bool> {
        Box::pin(async move {
            let call = ToolCall {
                attempt,
                call,
                tool,
                args,
            };
            tokio::time::timeout(
                self.wait,
                crate::service::preflight::confirm(
                    self.ledger.as_ref(),
                    self.preflight.as_deref(),
                    call,
                    expected,
                ),
            )
            .await
            .map_err(|_| Error::Unavailable)?
        })
    }
}
