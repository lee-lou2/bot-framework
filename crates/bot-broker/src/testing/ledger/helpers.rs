//! Test helpers for manipulating `MemLedger` state (handovers, revocations, faults, resolutions).

use super::*;

impl MemLedger {
    pub fn add_run(&self, context: RunContext) {
        let generation = context.attempt.generation;
        let claim = context.attempt.claim.map_or(1, |epoch| epoch.0);
        let run = context.attempt.run.clone();
        self.peek(|i| {
            i.runs.insert(
                run,
                RunEntry {
                    context,
                    generation,
                    claim,
                    cancelled: false,
                    ended: false,
                },
            );
        });
    }

    /// Simulates a crash/conflict handover by advancing both generation and claim epoch.
    pub fn bump_generation(&self, run: &str) -> Generation {
        self.peek(|i| {
            let entry = i.runs.get_mut(&RunId(run.into())).expect("run");
            entry.generation = Generation(entry.generation.0 + 1);
            entry.claim += 1;
            entry.generation
        })
    }

    /// Simulates a normal resume within the same generation by advancing only claim epoch.
    pub fn bump_claim(&self, run: &str) -> ClaimEpoch {
        self.peek(|i| {
            let entry = i.runs.get_mut(&RunId(run.into())).expect("run");
            entry.claim += 1;
            ClaimEpoch(entry.claim)
        })
    }

    pub fn claim_epoch(&self, run: &str) -> ClaimEpoch {
        self.peek(|i| ClaimEpoch(i.runs[&RunId(run.into())].claim))
    }

    pub fn set_after_begin(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.after_begin.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(hook));
    }

    pub fn set_active_revision(&self, profile: &str, revision: &str) {
        self.peek(|i| {
            i.active
                .insert(ProfileId(profile.into()), Revision(revision.into()));
        });
    }

    pub fn set_before_start(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.before_start.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(hook));
    }

    pub fn set_after_finish(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.after_finish.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(hook));
    }

    pub(super) fn fire_after_begin(&self) {
        let hook = self
            .after_begin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub fn cancel(&self, run: &str) {
        self.peek(|i| i.runs.get_mut(&RunId(run.into())).expect("run").cancelled = true);
    }

    /// Marks the run ended and releases its lease so subsequent worker calls return `Stale`.
    pub fn end_run(&self, run: &str) {
        self.peek(|i| i.runs.get_mut(&RunId(run.into())).expect("run").ended = true);
    }

    pub fn approval_states_of(&self, run: &str, call: &str) -> Vec<ApprovalState> {
        let key = (RunId(run.into()), CallId(call.into()));
        self.peek(|i| {
            i.approvals
                .values()
                .filter(|entry| entry.key == key)
                .map(|entry| entry.stored.state)
                .collect()
        })
    }

    pub fn approval_ids(&self, run: &str) -> Vec<ApprovalId> {
        let run = RunId(run.into());
        self.peek(|i| {
            i.approvals
                .iter()
                .filter(|(_, entry)| entry.key.0 == run)
                .map(|(nonce, _)| nonce.clone())
                .collect()
        })
    }

    pub fn approval_lookup(&self, nonce: &ApprovalId) -> Option<StoredApproval> {
        self.peek(|i| i.approvals.get(nonce).map(|entry| entry.stored.clone()))
    }

    /// Checks profile, generation, cancellation, and ended state without requiring an active claim.
    pub fn identity_ok(&self, attempt: &Attempt) -> Result<(), Error> {
        self.peek(|i| i.identity(attempt).map(|_| ()))
    }

    pub fn supersede_open_approvals(&self, run: &str, call: &str) {
        let key = (RunId(run.into()), CallId(call.into()));
        self.peek(|i| {
            for entry in i.approvals.values_mut().filter(|entry| entry.key == key) {
                if matches!(
                    entry.stored.state,
                    ApprovalState::Pending | ApprovalState::Approved
                ) {
                    entry.stored.state = ApprovalState::Superseded;
                }
            }
        });
    }

    pub fn finish_call_with(&self, run: &str, call: &str, receipt: &str) {
        self.peek(|i| {
            if let Some(record) = i.calls.get_mut(&(RunId(run.into()), CallId(call.into())))
                && !matches!(record.phase, CallPhase::Done(_))
            {
                record.phase = CallPhase::Done(JsonPayload(receipt.into()));
            }
        });
    }

    pub fn set_private_recall(&self, root: &str) {
        self.peek(|i| {
            i.private_roots.insert(RunId(root.into()));
        });
    }

    pub fn set_context_delay(&self, delay: std::time::Duration) {
        *self.context_delay.lock().unwrap_or_else(|e| e.into_inner()) = delay;
    }

    /// Blocks the executor thread on first poll of `context` to simulate a stalled poll.
    pub fn set_context_block(&self, block: std::time::Duration) {
        *self.context_block.lock().unwrap_or_else(|e| e.into_inner()) = block;
    }

    pub fn set_context_down(&self, down: bool) {
        self.context_down
            .store(down, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn hold_next_step(&self, prefix: &str) -> Arc<tokio::sync::Semaphore> {
        self.steps.hold(prefix)
    }

    pub async fn step_held(&self) {
        self.steps.entered().await;
    }

    pub fn after_next_step(&self, prefix: &str, hook: impl Fn() + Send + Sync + 'static) {
        self.steps.after(prefix, Arc::new(hook));
    }

    pub fn add_step(&self, run: &str, key: &str, body: &str) {
        self.peek(|i| {
            let steps = &mut i
                .runs
                .get_mut(&RunId(run.into()))
                .expect("run")
                .context
                .work
                .steps;
            steps.retain(|s| s.key != key);
            steps.push(bot_core::command::Step {
                key: key.into(),
                body: JsonPayload(body.into()),
            });
        });
    }

    pub fn remove_step(&self, run: &str, key: &str) {
        self.peek(|i| {
            i.runs
                .get_mut(&RunId(run.into()))
                .expect("run")
                .context
                .work
                .steps
                .retain(|s| s.key != key);
        });
    }

    pub fn step_keys(&self, run: &str) -> Vec<String> {
        self.peek(|i| {
            i.runs
                .get(&RunId(run.into()))
                .map(|entry| {
                    entry
                        .context
                        .work
                        .steps
                        .iter()
                        .map(|s| s.key.clone())
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// Makes `begin` return mismatched existing records so broker-side conflict checks can be tested.
    pub fn set_lenient_begin(&self, lenient: bool) {
        self.lenient_begin
            .store(lenient, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn force_done(&self, run: &str, call: &str, result: &str) {
        self.peek(|i| {
            i.calls
                .get_mut(&(RunId(run.into()), CallId(call.into())))
                .expect("call")
                .phase = CallPhase::Done(JsonPayload(result.into()));
        });
    }

    pub fn call_done(&self, run: &RunId, call: &CallId) -> bool {
        self.peek(|i| {
            i.calls
                .get(&(run.clone(), call.clone()))
                .is_some_and(|record| matches!(record.phase, CallPhase::Done(_)))
        })
    }

    pub fn phase(&self, run: &str, call: &str) -> CallPhase {
        self.peek(|i| {
            i.calls[&(RunId(run.into()), CallId(call.into()))]
                .phase
                .clone()
        })
    }

    pub fn call_hash(&self, run: &str, call: &str) -> [u8; 32] {
        self.peek(|i| i.calls[&(RunId(run.into()), CallId(call.into()))].args_sha256)
    }

    pub fn call_ids(&self, run: &str) -> Vec<String> {
        self.peek(|i| {
            i.calls
                .keys()
                .filter(|(r, _)| r.0 == run)
                .map(|(_, c)| c.0.clone())
                .collect()
        })
    }

    pub fn has_call(&self, run: &str, call: &str) -> bool {
        self.peek(|i| {
            i.calls
                .contains_key(&(RunId(run.into()), CallId(call.into())))
        })
    }

    pub fn pending_approval(&self, run: &str, call: &str) -> Option<ApprovalId> {
        match self.phase(run, call) {
            CallPhase::Open { approval } => approval,
            _ => None,
        }
    }

    pub fn approval_state(&self, nonce: &ApprovalId) -> ApprovalState {
        self.peek(|i| i.approvals[nonce].stored.state)
    }

    pub fn stored_approval(&self, nonce: &ApprovalId) -> StoredApproval {
        let stored = self.peek(|i| i.approvals[nonce].stored.clone());
        self.notes.with_receipt(stored, nonce)
    }

    pub fn approval_delivery(&self, nonce: &ApprovalId) -> Option<BindingId> {
        self.notes.delivery(nonce)
    }

    pub fn blocked_why(&self, run: &str, call: &str) -> Option<bot_core::api::ApprovalBlock> {
        self.notes.why(run, call)
    }

    pub fn post_receipt(&self, nonce: &ApprovalId, receipt: bot_core::cards::CardReceipt) {
        self.notes.post(nonce, receipt);
    }

    pub fn clear_receipt(&self, nonce: &ApprovalId) {
        self.notes.withdraw(nonce);
    }

    pub fn audit_log(&self) -> Vec<String> {
        self.peek(|i| i.audit.clone())
    }

    pub fn resolve(&self, run: &str, call: &str, resolution: Resolution) {
        self.peek(|i| {
            let record = i
                .calls
                .get_mut(&(RunId(run.into()), CallId(call.into())))
                .expect("call");
            match &mut record.phase {
                CallPhase::Held {
                    resolution: slot, ..
                } => *slot = Some(resolution),
                other => panic!("not held: {other:?}"),
            }
        });
    }

    pub fn woken(&self) -> Vec<(RunId, CallId)> {
        self.peek(|i| i.woken.clone())
    }

    pub fn force_started(&self, run: &str, call: &str, generation: u64, owner: &str) {
        self.peek(|i| {
            i.calls
                .get_mut(&(RunId(run.into()), CallId(call.into())))
                .expect("call")
                .phase = CallPhase::Started {
                generation: Generation(generation),
                owner: owner.into(),
                confirmed_step: false,
            };
        });
    }

    /// Atomic built-in commit (approval consumption + start + completion); returns `Ok(false)` if already done.
    pub fn complete_builtin(
        &self,
        start: StartCall,
        tool: &str,
        result: &str,
    ) -> Result<bool, Error> {
        let now = self.clock.now();
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let key = (start.attempt.run.clone(), start.call.clone());
        let record = i.calls.get(&key).ok_or(Error::Missing)?;
        if record.tool.0 != tool {
            return Err(Error::Conflict);
        }
        if matches!(record.phase, CallPhase::Done(_)) {
            return Ok(false);
        }
        Self::begin_run(&mut i, now, start)?;
        i.calls.get_mut(&key).expect("call").phase = CallPhase::Done(JsonPayload(result.into()));
        i.audit.push(format!("{tool}.done"));
        Ok(true)
    }

    pub fn finish_started(
        &self,
        attempt: &Attempt,
        call: &CallId,
        checked: &Revision,
        result: &str,
    ) -> Result<(), Error> {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        i.fence(attempt)?;
        if i.active.get(&attempt.profile) != Some(checked) {
            return Err(Error::Stale);
        }
        let key = (attempt.run.clone(), call.clone());
        let record = i.calls.get_mut(&key).ok_or(Error::Missing)?;
        match record.phase {
            CallPhase::Started { .. } => {
                record.phase = CallPhase::Done(JsonPayload(result.into()));
                i.audit.push("call.done".into());
                Ok(())
            }
            _ => Err(Error::Conflict),
        }
    }

    pub fn principal_of(&self, attempt: &Attempt) -> Result<PrincipalId, Error> {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(i.fence(attempt)?.context.principal.clone())
    }

    pub fn check_current(&self, attempt: &Attempt, checked: &Revision) -> Result<(), Error> {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        i.fence(attempt)?;
        if i.active.get(&attempt.profile) == Some(checked) {
            Ok(())
        } else {
            Err(Error::Stale)
        }
    }
}
