//! In-memory reference implementation of `CallLedger` enforcing all production store invariants.
//!
//! - Worker operations require current generation, claim epoch, and active (non-cancelled, non-ended) run; approval decisions check identity only.
//! - `start` atomically checks `checked_revision` against active profile revision, consumes any bound approval, checks the private-memory flag, and transitions `Open` -> `Started`.
//! - `open_approval` supersedes prior pending/approved entries; `decide` and `supersede` wake waiting runs.
//! - Every state transition appends an audit entry within the same mutex guard.

use crate::{approval::same_binding, sys::Clock};
use bot_core::{
    api::ApprovalBlock,
    command::JsonPayload,
    id::*,
    ledger::*,
    port::{Error, Reply},
    state::{Approval, RunContext},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

pub struct MemLedger {
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
    after_begin: Mutex<Option<Hook>>,
    before_start: Mutex<Option<Hook>>,
    after_finish: Mutex<Option<Hook>>,
    context_down: AtomicBool,
    context_delay: Mutex<std::time::Duration>,
    /// Blocks the executor thread during `context` polling to simulate scheduling lag past deadlines.
    context_block: Mutex<std::time::Duration>,
    /// When set, `begin` returns existing records without checking tool/args_sha256 mismatch.
    lenient_begin: AtomicBool,
    steps: steps::StepProbe,
    notes: openai::Notes,
}

type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Inner {
    runs: HashMap<RunId, RunEntry>,
    calls: HashMap<(RunId, CallId), CallRecord>,
    approvals: HashMap<ApprovalId, ApprovalEntry>,
    audit: Vec<String>,
    private_roots: HashSet<RunId>,
    woken: Vec<(RunId, CallId)>,
    active: HashMap<ProfileId, Revision>,
}

struct RunEntry {
    context: RunContext,
    generation: Generation,
    claim: u64,
    cancelled: bool,
    ended: bool,
}

struct ApprovalEntry {
    stored: StoredApproval,
    key: (RunId, CallId),
}

impl Inner {
    /// Checks run identity (profile, generation, active) without requiring a claim epoch (approvals/UI/tracing).
    fn identity(&self, attempt: &Attempt) -> Result<&RunEntry, Error> {
        let run = self.runs.get(&attempt.run).ok_or(Error::Missing)?;
        if run.context.attempt.profile != attempt.profile {
            return Err(Error::Denied);
        }
        if run.generation != attempt.generation || run.cancelled || run.ended {
            return Err(Error::Stale);
        }
        Ok(run)
    }

    /// Checks run identity plus current claim epoch ownership for worker operations.
    fn fence(&self, attempt: &Attempt) -> Result<&RunEntry, Error> {
        let run = self.identity(attempt)?;
        match attempt.claim {
            None => Err(Error::Denied),
            Some(epoch) if epoch.0 != run.claim => Err(Error::Stale),
            Some(_) => Ok(run),
        }
    }

    fn private_flag(&self, run: &RunId) -> bool {
        self.runs
            .get(run)
            .is_some_and(|r| self.private_roots.contains(&r.context.root))
    }
}

impl MemLedger {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            inner: Mutex::default(),
            after_begin: Mutex::new(None),
            before_start: Mutex::new(None),
            after_finish: Mutex::new(None),
            context_down: AtomicBool::new(false),
            lenient_begin: AtomicBool::new(false),
            context_delay: Mutex::new(std::time::Duration::ZERO),
            context_block: Mutex::new(std::time::Duration::ZERO),
            steps: steps::StepProbe::default(),
            notes: openai::Notes::default(),
        }
    }

    fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Inner, SystemTime) -> Result<T, Error>,
    ) -> Reply<'_, T> {
        let now = self.clock.now();
        let result = f(
            &mut self.inner.lock().unwrap_or_else(|e| e.into_inner()),
            now,
        );
        Box::pin(async move { result })
    }

    fn peek<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        f(&mut self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

mod helpers;
mod openai;
mod steps;

impl MemLedger {
    fn begin_run(i: &mut Inner, now: SystemTime, start: StartCall) -> Result<(), Error> {
        i.fence(&start.attempt)?;
        if i.active.get(&start.attempt.profile) != Some(&start.checked_revision) {
            return Err(Error::Stale);
        }
        let key = (start.attempt.run.clone(), start.call.clone());
        let phase = i.calls.get(&key).ok_or(Error::Missing)?.phase.clone();
        if !matches!(phase, CallPhase::Open { .. }) {
            return Err(Error::Conflict);
        }
        if i.private_flag(&start.attempt.run) && !start.private_recall {
            return Err(Error::Conflict);
        }
        if let Some((expected, reasons)) = &start.approval {
            let entry = i.approvals.get(&expected.nonce).ok_or(Error::Missing)?;
            if entry.key != key {
                return Err(Error::Denied);
            }
            match entry.stored.state {
                ApprovalState::Approved => {}
                ApprovalState::Consumed => return Err(Error::Conflict),
                ApprovalState::Pending | ApprovalState::Denied | ApprovalState::Revoked => {
                    return Err(Error::Denied);
                }
                ApprovalState::Expired | ApprovalState::Superseded => return Err(Error::Stale),
            }
            if now >= entry.stored.approval.expires_at
                || !same_binding(&entry.stored.approval, expected)
                || &entry.stored.reasons != reasons
            {
                return Err(Error::Stale);
            }
            i.approvals
                .get_mut(&expected.nonce)
                .expect("approval")
                .stored
                .state = ApprovalState::Consumed;
        }
        i.calls.get_mut(&key).expect("call").phase = CallPhase::Started {
            generation: start.attempt.generation,
            owner: start.owner,
            confirmed_step: false,
        };
        i.audit.push("call.start".into());
        Ok(())
    }

    /// Atomically verifies the bound digest against `expected`, consumes approval, and completes a `memory.share` call.
    pub fn complete_share(
        &self,
        start: StartCall,
        expected: [u8; 32],
        id: &str,
    ) -> Result<(), Error> {
        let now = self.clock.now();
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let key = (start.attempt.run.clone(), start.call.clone());
        let record = i.calls.get(&key).ok_or(Error::Missing)?;
        if matches!(record.phase, CallPhase::Done(_)) {
            return Ok(());
        }
        if record.args_sha256 != expected {
            return Err(Error::Conflict);
        }
        Self::begin_run(&mut i, now, start)?;
        i.calls.get_mut(&key).expect("call").phase =
            CallPhase::Done(JsonPayload(format!("{{\"id\":\"{id}\"}}")));
        i.audit.push("memory.shared".into());
        Ok(())
    }
}

impl CallLedger for MemLedger {
    fn context(&self, attempt: Attempt) -> Reply<'_, RunContext> {
        let down = self.context_down.load(Ordering::SeqCst);
        let delay = *self.context_delay.lock().unwrap_or_else(|e| e.into_inner());
        let block = *self.context_block.lock().unwrap_or_else(|e| e.into_inner());
        let reply = self.with(move |i, _| {
            if down {
                return Err(Error::Unavailable);
            }
            let entry = i.fence(&attempt)?;
            let mut context = entry.context.clone();
            context.private_memory_read = i.private_flag(&attempt.run);
            Ok(context)
        });
        if delay.is_zero() && block.is_zero() {
            return reply;
        }
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if !block.is_zero() {
                std::thread::sleep(block);
            }
            reply.await
        })
    }

    fn lookup(&self, attempt: Attempt, call: CallId) -> Reply<'_, Option<CallRecord>> {
        self.with(move |i, _| {
            i.fence(&attempt)?;
            Ok(i.calls.get(&(attempt.run.clone(), call)).cloned())
        })
    }

    fn step(&self, attempt: Attempt, key: String) -> Reply<'_, Option<JsonPayload>> {
        let down = self.context_down.load(Ordering::SeqCst);
        let delay = *self.context_delay.lock().unwrap_or_else(|e| e.into_inner());
        let probe = self.steps.take(&key);
        let reply = self.with(move |i, _| {
            if down {
                return Err(Error::Unavailable);
            }
            let entry = i.fence(&attempt)?;
            Ok(entry
                .context
                .work
                .steps
                .iter()
                .find(|s| s.key == key)
                .map(|s| s.body.clone()))
        });
        self.steps.wrap(probe, delay, reply)
    }

    fn active_revision(&self, attempt: Attempt) -> Reply<'_, Option<Revision>> {
        let down = self.context_down.load(Ordering::SeqCst);
        self.with(move |i, _| {
            if down {
                return Err(Error::Unavailable);
            }
            i.fence(&attempt)?;
            Ok(i.active.get(&attempt.profile).cloned())
        })
    }

    fn begin(
        &self,
        attempt: Attempt,
        call: CallId,
        tool: ToolId,
        args_sha256: [u8; 32],
    ) -> Reply<'_, CallRecord> {
        let lenient = self.lenient_begin.load(Ordering::SeqCst);
        let reply = self.with(move |i, _| {
            i.fence(&attempt)?;
            let key = (attempt.run.clone(), call.clone());
            if let Some(existing) = i.calls.get(&key) {
                if !lenient && (existing.tool != tool || existing.args_sha256 != args_sha256) {
                    return Err(Error::Conflict);
                }
                return Ok(existing.clone());
            }
            let record = CallRecord {
                call,
                tool,
                args_sha256,
                phase: CallPhase::Open { approval: None },
            };
            i.calls.insert(key, record.clone());
            Ok(record)
        });
        self.fire_after_begin();
        reply
    }

    fn open_approval(&self, open: OpenApproval) -> Reply<'_, ()> {
        self.with(|i, _| {
            let a = &open.approval;
            i.fence(&a.attempt)?;
            let key = (a.attempt.run.clone(), a.call.clone());
            let record = i.calls.get(&key).ok_or(Error::Missing)?;
            let CallPhase::Open { approval: previous } = record.phase.clone() else {
                return Err(Error::Conflict);
            };
            if let Some(old) = previous.and_then(|n| i.approvals.get_mut(&n))
                && matches!(
                    old.stored.state,
                    ApprovalState::Pending | ApprovalState::Approved
                )
            {
                old.stored.state = ApprovalState::Superseded;
            }
            i.calls.get_mut(&key).expect("call").phase = CallPhase::Open {
                approval: Some(a.nonce.clone()),
            };
            self.notes.opened(&a.nonce, open.delivery.as_ref());
            i.approvals.insert(
                a.nonce.clone(),
                ApprovalEntry {
                    stored: StoredApproval {
                        // Approvals bind to generation identity only (never claim epoch).
                        approval: Approval {
                            attempt: Attempt {
                                claim: None,
                                ..open.approval.attempt.clone()
                            },
                            ..open.approval.clone()
                        },
                        reasons: open.reasons,
                        card: open.card,
                        state: ApprovalState::Pending,
                        delivery: None,
                    },
                    key,
                },
            );
            i.audit.push("approval.open".into());
            Ok(())
        })
    }

    fn approval_context(&self, nonce: ApprovalId, actor: PrincipalId) -> Reply<'_, RunContext> {
        self.with(|i, _| {
            let entry = i.approvals.get(&nonce).ok_or(Error::Missing)?;
            if entry.stored.approval.approver != actor {
                return Err(Error::Denied);
            }
            let run = i.identity(&entry.stored.approval.attempt)?;
            let mut context = run.context.clone();
            context.attempt.claim = None;
            context.private_memory_read = i.private_flag(&entry.stored.approval.attempt.run);
            Ok(context)
        })
    }

    fn approval(&self, nonce: ApprovalId) -> Reply<'_, StoredApproval> {
        self.with(|i, _| {
            let found = i.approvals.get(&nonce).map(|e| e.stored.clone());
            found
                .map(|s| self.notes.with_receipt(s, &nonce))
                .ok_or(Error::Missing)
        })
    }

    fn finish_blocked(&self, f: FinishCall, why: ApprovalBlock) -> Reply<'_, ()> {
        let (run, call, done) = (f.attempt.run.clone(), f.call.clone(), self.finish(f));
        Box::pin(async move { done.await.map(|()| self.notes.blocked(&run, &call, why)) })
    }

    fn supersede(&self, nonce: ApprovalId, actor: PrincipalId) -> Reply<'_, ()> {
        self.with(|i, _| {
            let entry = i.approvals.get(&nonce).ok_or(Error::Missing)?;
            if entry.stored.approval.approver != actor {
                return Err(Error::Denied);
            }
            if entry.stored.state == ApprovalState::Superseded {
                return Ok(());
            }
            i.identity(&entry.stored.approval.attempt)?;
            let key = entry.key.clone();
            let open = i.calls.get(&key).ok_or(Error::Missing)?;
            if !matches!(entry.stored.state, ApprovalState::Pending | ApprovalState::Approved)
                || !matches!(&open.phase, CallPhase::Open { approval: Some(active) } if active == &nonce)
            {
                return Err(Error::Conflict);
            }
            i.approvals.get_mut(&nonce).expect("approval").stored.state = ApprovalState::Superseded;
            i.woken.push(key);
            i.audit.push("approval.superseded".into());
            Ok(())
        })
    }

    fn decide(&self, decision: Decision) -> Reply<'_, ApprovalState> {
        self.with(|i, now| {
            let entry = i.approvals.get(&decision.nonce).ok_or(Error::Missing)?;
            if decision.actor != entry.stored.approval.approver {
                return Err(Error::Denied);
            }
            i.identity(&entry.stored.approval.attempt)?;
            if now >= entry.stored.approval.expires_at {
                return Err(Error::Stale);
            }
            let next = match (entry.stored.state, decision.allow) {
                (ApprovalState::Pending, true) => ApprovalState::Approved,
                (ApprovalState::Pending, false) => ApprovalState::Denied,
                (ApprovalState::Approved, false) => ApprovalState::Revoked,
                _ => return Err(Error::Conflict),
            };
            let entry = i.approvals.get_mut(&decision.nonce).expect("approval");
            entry.stored.state = next;
            let key = entry.key.clone();
            i.woken.push(key);
            i.audit.push(format!("approval.{next:?}").to_lowercase());
            Ok(next)
        })
    }

    fn start(&self, start: StartCall) -> Reply<'_, ()> {
        let hook = self
            .before_start
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook();
        }
        self.with(|i, now| {
            let key = (start.attempt.run.clone(), start.call.clone());
            // Built-in atomic commits must use their dedicated store methods, never generic `start`.
            if i.calls.get(&key).is_some_and(|c| {
                matches!(
                    c.tool.0.as_str(),
                    "memory.share" | "schedule.create" | "schedule.cancel" | "schedule.deliver"
                )
            }) {
                return Err(Error::Denied);
            }
            Self::begin_run(i, now, start)
        })
    }

    fn finish(&self, finish: FinishCall) -> Reply<'_, ()> {
        let reply = self.with(|i, _| {
            i.fence(&finish.attempt)?;
            let key = (finish.attempt.run.clone(), finish.call.clone());
            let phase = i.calls.get(&key).ok_or(Error::Missing)?.phase.clone();
            let outcome = &finish.outcome;
            let allowed = match &phase {
                CallPhase::Open { .. } => matches!(outcome, Outcome::Blocked),
                CallPhase::Started { generation, .. } => {
                    generation.0 <= finish.attempt.generation.0
                }
                CallPhase::Held {
                    resolution: Some(Resolution::Executed { .. }),
                    ..
                } => {
                    matches!(outcome, Outcome::Done(_))
                }
                CallPhase::Held {
                    resolution: Some(Resolution::NotExecuted),
                    ..
                } => {
                    matches!(outcome, Outcome::Released | Outcome::Blocked)
                }
                _ => false,
            };
            if !allowed {
                return Err(Error::Conflict);
            }
            let (next, label): (CallPhase, &str) = match finish.outcome {
                Outcome::Done(payload) => (CallPhase::Done(payload), "call.done"),
                Outcome::Released => (CallPhase::Open { approval: None }, "call.released"),
                Outcome::Held(reason) => (
                    CallPhase::Held {
                        reason,
                        resolution: None,
                    },
                    "call.held",
                ),
                Outcome::Blocked => (CallPhase::Denied, "call.blocked"),
            };
            i.calls.get_mut(&key).expect("call").phase = next;
            i.audit.push(label.into());
            Ok(())
        });
        let hook = self
            .after_finish
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook();
        }
        reply
    }
    fn start_with(
        &self,
        _: StartCall,
        _: bot_core::grant::Authority,
    ) -> Reply<'_, bot_core::grant::Granted> {
        Box::pin(async { Err(Error::Denied) })
    }

    fn evidence(&self, _: Attempt) -> Reply<'_, Option<bot_core::grant::Evidence>> {
        Box::pin(async { Ok(None) })
    }

    fn finish_pass(
        &self,
        _: FinishCall,
        _: u32,
        _: std::collections::BTreeSet<AgentId>,
    ) -> Reply<'_, bot_core::pass::PassNext> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}
