//! In-memory `PrivateLedger` test double mirroring store lease, fingerprint, and revision invariants.

use bot_core::{
    id::*,
    intake::RequestKey,
    ledger::*,
    port::{Error, Reply},
    profile::Scopes,
    state::Conversation,
};
use std::{collections::HashMap, sync::Mutex};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Reserved,
    Released,
    Started,
    Completed,
    Passed,
    Rejected,
    Held,
}

struct Row {
    fingerprint: [u8; 32],
    allow_pass: bool,
    agent: AgentId,
    tool: ToolId,
    conversation: (String, String, Option<String>),
    phase: Phase,
    attempt: Attempt,
    epoch: u64,
}

#[derive(Clone, Debug)]
pub struct Stored {
    pub key: RequestKey,
    pub fingerprint: [u8; 32],
    pub agent: AgentId,
    pub tool: ToolId,
    pub scopes: Scopes,
}

#[derive(Default)]
pub struct MemPrivateLedger {
    rows: Mutex<HashMap<RequestKey, Row>>,
    stored: Mutex<Vec<Stored>>,
    base: Mutex<Option<Attempt>>,
    active: Mutex<Option<Revision>>,
    starts: Mutex<Vec<(ToolId, [u8; 32], Revision)>>,
    outcomes: Mutex<Vec<&'static str>>,
    before_start: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
}

use super::lock;

fn conversation_key(c: &Conversation) -> (String, String, Option<String>) {
    (format!("{:?}", c.kind), c.id.clone(), c.thread.clone())
}

fn label(outcome: &PrivateOutcome) -> &'static str {
    match outcome {
        PrivateOutcome::RetryBeforeStart => "retry_before_start",
        PrivateOutcome::NotSent => "not_sent",
        PrivateOutcome::Completed => "completed",
        PrivateOutcome::Passed => "passed",
        PrivateOutcome::Held => "held",
        PrivateOutcome::Rejected => "rejected",
    }
}

impl MemPrivateLedger {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_attempt(&self, attempt: Attempt) {
        *lock(&self.base) = Some(attempt);
    }
    pub fn set_active_revision(&self, revision: &str) {
        *lock(&self.active) = Some(Revision(revision.into()));
    }
    pub fn stored(&self) -> Vec<Stored> {
        lock(&self.stored).clone()
    }
    pub fn outcomes(&self) -> Vec<&'static str> {
        lock(&self.outcomes).clone()
    }
    pub fn phases(&self) -> Vec<String> {
        lock(&self.rows)
            .values()
            .map(|r| format!("{:?}", r.phase))
            .collect()
    }
    pub fn starts(&self) -> Vec<(ToolId, [u8; 32], Revision)> {
        lock(&self.starts).clone()
    }
    pub fn set_before_start(&self, hook: impl Fn() + Send + Sync + 'static) {
        *lock(&self.before_start) = Some(std::sync::Arc::new(hook));
    }
    /// Simulates lease-expiry sweep turning started rows into durable `Held`.
    pub fn expire_started(&self) {
        for row in lock(&self.rows).values_mut() {
            if row.phase == Phase::Started {
                row.phase = Phase::Held;
            }
        }
    }
}

impl PrivateLedger for MemPrivateLedger {
    fn begin(&self, request: PrivateRequest) -> Reply<'_, PrivatePermit> {
        let result = (|| {
            if lock(&self.active)
                .as_ref()
                .is_some_and(|a| *a != request.revision)
            {
                return Err(Error::Stale);
            }
            let mut rows = lock(&self.rows);
            let conversation = conversation_key(&request.conversation);
            if let Some(row) = rows.get_mut(&request.key) {
                let fingerprint = if row.allow_pass {
                    request.pass_fingerprint.ok_or(Error::Conflict)?
                } else {
                    request.fingerprint
                };
                let same = row.fingerprint == fingerprint
                    && row.agent == request.agent
                    && row.tool == request.tool
                    && row.conversation == conversation;
                if !same {
                    return Err(Error::Conflict);
                }
                return Ok(match row.phase {
                    Phase::Released => {
                        row.phase = Phase::Reserved;
                        row.epoch += 1;
                        row.attempt.claim = Some(ClaimEpoch(row.epoch));
                        PrivatePermit::Start {
                            attempt: row.attempt.clone(),
                            allow_pass: row.allow_pass,
                        }
                    }
                    Phase::Reserved | Phase::Started => PrivatePermit::InProgress,
                    Phase::Completed => PrivatePermit::Completed,
                    Phase::Passed => PrivatePermit::Passed,
                    Phase::Rejected => PrivatePermit::Rejected,
                    Phase::Held => PrivatePermit::Held,
                });
            }
            let mut attempt = lock(&self.base).clone().ok_or(Error::Unavailable)?;
            let allow_pass = request.pass_fingerprint.is_some();
            let fingerprint = request.pass_fingerprint.unwrap_or(request.fingerprint);
            let epoch = attempt.claim.map_or(1, |c| c.0);
            attempt.claim = Some(ClaimEpoch(epoch));
            lock(&self.stored).push(Stored {
                key: request.key.clone(),
                fingerprint,
                agent: request.agent.clone(),
                tool: request.tool.clone(),
                scopes: request.scopes,
            });
            rows.insert(
                request.key,
                Row {
                    fingerprint,
                    allow_pass,
                    agent: request.agent,
                    tool: request.tool,
                    conversation,
                    phase: Phase::Reserved,
                    attempt: attempt.clone(),
                    epoch,
                },
            );
            Ok(PrivatePermit::Start {
                attempt,
                allow_pass,
            })
        })();
        Box::pin(async move { result })
    }

    fn start(&self, start: PrivateStart) -> Reply<'_, ()> {
        let hook = lock(&self.before_start).clone();
        if let Some(hook) = hook {
            hook();
        }
        let result = (|| {
            let mut rows = lock(&self.rows);
            let row = rows
                .values_mut()
                .find(|r| r.attempt.run == start.attempt.run)
                .ok_or(Error::Missing)?;
            if row.attempt.claim != start.attempt.claim
                || row.attempt.generation != start.attempt.generation
            {
                return Err(Error::Stale);
            }
            if row.phase != Phase::Reserved {
                return Err(Error::Conflict);
            }
            if row.tool != start.tool || row.fingerprint != start.fingerprint {
                return Err(Error::Denied);
            }
            // Active revision must explicitly match `checked_revision`; unset/missing fails closed.
            if lock(&self.active).as_ref() != Some(&start.checked_revision) {
                return Err(Error::Stale);
            }
            row.phase = Phase::Started;
            lock(&self.starts).push((start.tool, start.fingerprint, start.checked_revision));
            Ok(())
        })();
        Box::pin(async move { result })
    }

    fn finish(&self, permit: Attempt, outcome: PrivateOutcome) -> Reply<'_, ()> {
        let result = (|| {
            let mut rows = lock(&self.rows);
            let row = rows
                .values_mut()
                .find(|r| r.attempt.run == permit.run)
                .ok_or(Error::Missing)?;
            if row.attempt.claim != permit.claim || row.attempt.generation != permit.generation {
                return Err(Error::Stale);
            }
            let next = match (row.phase, &outcome) {
                (Phase::Reserved, PrivateOutcome::RetryBeforeStart) => Phase::Released,
                (Phase::Reserved, PrivateOutcome::Rejected) => Phase::Rejected,
                (Phase::Started, PrivateOutcome::NotSent) => Phase::Released,
                (Phase::Started, PrivateOutcome::Completed) => Phase::Completed,
                (Phase::Started | Phase::Held, PrivateOutcome::Passed) if row.allow_pass => {
                    Phase::Passed
                }
                (Phase::Started, PrivateOutcome::Held) => Phase::Held,
                (Phase::Started, PrivateOutcome::Rejected) => Phase::Rejected,
                _ => return Err(Error::Conflict),
            };
            lock(&self.outcomes).push(label(&outcome));
            row.phase = next;
            Ok(())
        })();
        Box::pin(async move { result })
    }
}
