//! 시험이 원장 상태를 직접 만지는 도우미(인계·취소·철회·보류 판정 등).

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

    /// 소유권이 넘어간 것처럼 세대를 올린다(충돌·크래시 인계). claim epoch도 함께 오른다.
    pub fn bump_generation(&self, run: &str) -> Generation {
        self.peek(|i| {
            let entry = i.runs.get_mut(&RunId(run.into())).expect("run");
            entry.generation = Generation(entry.generation.0 + 1);
            entry.claim += 1;
            entry.generation
        })
    }

    /// 같은 세대에서 새 리스를 잡은 것처럼 claim epoch만 올린다(승인 대기 뒤 정상 재개).
    pub fn bump_claim(&self, run: &str) -> ClaimEpoch {
        self.peek(|i| {
            let entry = i.runs.get_mut(&RunId(run.into())).expect("run");
            entry.claim += 1;
            ClaimEpoch(entry.claim)
        })
    }

    /// 지금 이 run의 claim epoch.
    pub fn claim_epoch(&self, run: &str) -> ClaimEpoch {
        self.peek(|i| ClaimEpoch(i.runs[&RunId(run.into())].claim))
    }

    /// `begin`이 끝난 직후(판정과 시작 사이) 한 번씩 실행할 동작을 건다. 그 사이의 철회를 흉내 낸다.
    pub fn set_after_begin(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.after_begin.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(hook));
    }

    /// 저장소가 활성 판본을 아는 것처럼 정한다(`start`가 `checked_revision`과 대조한다).
    pub fn set_active_revision(&self, profile: &str, revision: &str) {
        self.peek(|i| {
            i.active
                .insert(ProfileId(profile.into()), Revision(revision.into()));
        });
    }

    /// `start`가 판본을 대조하기 직전에 한 번씩 실행할 동작을 건다(브로커의 마지막 재확인 뒤의 철회를 흉내 낸다).
    pub fn set_before_start(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.before_start.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(hook));
    }

    /// `finish`가 끝난 직후(결과를 보존한 직후)에 한 번씩 실행할 동작을 건다.
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

    /// run이 성공으로 끝나 리스를 놓았다(예약 회신 전송의 커밋처럼 run 완료를 함께 쓰는 연산 뒤): 이 run의 워커 연산은 `Stale`이다.
    pub fn end_run(&self, run: &str) {
        self.peek(|i| i.runs.get_mut(&RunId(run.into())).expect("run").ended = true);
    }

    /// 이 호출에 묶인 승인들의 상태(가짜 저장소가 «소유자 본인의 거절·철회 결정이 있는가»를 볼 때 쓴다).
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

    /// 이 run에 열린 승인의 nonce들(카드를 열었는지 보는 시험이 쓴다).
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

    /// 승인 하나(없으면 `None`). `stored_approval`과 달리 없어도 멈추지 않는다.
    pub fn approval_lookup(&self, nonce: &ApprovalId) -> Option<StoredApproval> {
        self.peek(|i| i.approvals.get(nonce).map(|entry| entry.stored.clone()))
    }

    /// 이 attempt의 신원(프로필·세대·취소·종료)만 확인한다. claim은 보지 않는다(승인 대기 중에는 쥔 claim이 없다).
    pub fn identity_ok(&self, attempt: &Attempt) -> Result<(), Error> {
        self.peek(|i| i.identity(attempt).map(|_| ()))
    }

    /// 이 호출의 대기·승인된 승인을 모두 무효로 한다(미발송 종결이 옛 카드를 닫는다).
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

    /// 호출 기록이 있고 끝나지 않았으면 영수증으로 끝낸다(없으면 아무것도 하지 않는다).
    pub fn finish_call_with(&self, run: &str, call: &str, receipt: &str) {
        self.peek(|i| {
            if let Some(record) = i.calls.get_mut(&(RunId(run.into()), CallId(call.into())))
                && !matches!(record.phase, CallPhase::Done(_))
            {
                record.phase = CallPhase::Done(JsonPayload(receipt.into()));
            }
        });
    }

    /// 요청 트리(루트 run)의 private 기억 조회 플래그를 켠다.
    pub fn set_private_recall(&self, root: &str) {
        self.peek(|i| {
            i.private_roots.insert(RunId(root.into()));
        });
    }

    /// 이후 `context`가 `delay`만큼 느리게 답한다(느린 원장).
    pub fn set_context_delay(&self, delay: std::time::Duration) {
        *self.context_delay.lock().unwrap_or_else(|e| e.into_inner()) = delay;
    }

    /// 이후 `context`가 응답을 처음 폴링할 때 스레드를 `block`만큼 붙잡는다(실행이 밀린 상황).
    pub fn set_context_block(&self, block: std::time::Duration) {
        *self.context_block.lock().unwrap_or_else(|e| e.into_inner()) = block;
    }

    /// 원장 장애를 흉내 낸다: 켜 두면 `context`가 `Unavailable`로 답한다.
    pub fn set_context_down(&self, down: bool) {
        self.context_down
            .store(down, std::sync::atomic::Ordering::SeqCst);
    }

    /// 다음에 `prefix`로 시작하는 키를 읽는 `step`을 붙잡는다(읽는 도중에 끼어드는 시험). 돌려준 문에 `add_permits(1)`을 하면 풀린다. 한 번 쓰면 없어진다.
    pub fn hold_next_step(&self, prefix: &str) -> Arc<tokio::sync::Semaphore> {
        self.steps.hold(prefix)
    }

    /// 붙잡힌 읽기가 들어설 때까지 기다린다(이미 들어섰다면 바로 돌아온다).
    pub async fn step_held(&self) {
        self.steps.entered().await;
    }

    /// 다음에 `prefix`로 시작하는 키를 읽는 `step`이 값을 돌려주는 **바로 그 순간**(같은 폴링 안)에 `hook`을 한다. 한 번 쓰면 없어진다.
    pub fn after_next_step(&self, prefix: &str, hook: impl Fn() + Send + Sync + 'static) {
        self.steps.after(prefix, Arc::new(hook));
    }

    /// 체크포인트 한 칸을 run에 더한다(`context().work.steps`로 읽힌다). 같은 키는 덮어쓴다.
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

    /// 체크포인트 한 칸을 지운다(시험이 기록을 일부러 망가뜨릴 때 쓴다. 실제 저장소의 체크포인트는 지워지지 않는다).
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

    /// 이 run의 체크포인트 키들(기록 순서).
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

    /// 저장소가 도구·인자 해시가 다른 기존 호출도 그대로 돌려주는 것처럼 만든다(브로커가 한 번 더 거르는지 본다).
    pub fn set_lenient_begin(&self, lenient: bool) {
        self.lenient_begin
            .store(lenient, std::sync::atomic::Ordering::SeqCst);
    }

    /// 다른 프로세스가 이 호출을 먼저 끝낸 것처럼 `Done`으로 만든다.
    pub fn force_done(&self, run: &str, call: &str, result: &str) {
        self.peek(|i| {
            i.calls
                .get_mut(&(RunId(run.into()), CallId(call.into())))
                .expect("call")
                .phase = CallPhase::Done(JsonPayload(result.into()));
        });
    }

    /// 이 호출이 이미 완료됐는가(기억 공유의 재전달 판정에 쓴다).
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

    /// 호출 장부에 남은 인자 해시.
    pub fn call_hash(&self, run: &str, call: &str) -> [u8; 32] {
        self.peek(|i| i.calls[&(RunId(run.into()), CallId(call.into()))].args_sha256)
    }

    /// 장부에 남은 호출 ID들.
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

    /// 호출이 `Open`이고 가장 최근 승인이 있을 때 그 nonce.
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

    /// 이 승인 카드를 보내라고 브로커가 실어 준 전달 바인딩(OpenAI 출처의 Slack 승인 경로).
    pub fn approval_delivery(&self, nonce: &ApprovalId) -> Option<BindingId> {
        self.notes.delivery(nonce)
    }

    /// 승인 경로 때문에 막은 호출의 저장된 이유(`finish_blocked`). 이유 없이 막았거나 막지 않았으면 `None`이다.
    pub fn blocked_why(&self, run: &str, call: &str) -> Option<bot_core::api::ApprovalBlock> {
        self.notes.why(run, call)
    }

    /// 게이트웨이가 카드 게시를 확인해 영수증을 확정한 것처럼 기록한다(`StoredApproval::delivery`).
    pub fn post_receipt(&self, nonce: &ApprovalId, receipt: bot_core::cards::CardReceipt) {
        self.notes.post(nonce, receipt);
    }

    /// 확정한 영수증이 사라진 승인을 만든다(저장소는 영수증 없이 승인되지 않지만, 브로커가 어디에 올라갔는지 모르는 카드를 닫힌 채 막는지 본다).
    pub fn clear_receipt(&self, nonce: &ApprovalId) {
        self.notes.withdraw(nonce);
    }

    pub fn audit_log(&self) -> Vec<String> {
        self.peek(|i| i.audit.clone())
    }

    /// 관리자가 보류를 판정한 것처럼 기록한다.
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
                other => panic!("보류가 아니다: {other:?}"),
            }
        });
    }

    pub fn woken(&self) -> Vec<(RunId, CallId)> {
        self.peek(|i| i.woken.clone())
    }

    /// 시험이 호출을 직접 `Started`로 만든다(옛 세대가 시작만 하고 사라진 상황).
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

    /// 내장 도구 하나의 저장소 쪽 커밋: 승인 소비·시작·완료를 한 번에 한다(`StartCall` 검사는 일반 `start`와 같다). 호출이 이미
    /// 완료됐으면 아무것도 바꾸지 않고 `Ok(false)`다. 호출을 기록한 도구가 `tool`이 아니면 `Conflict`다.
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

    /// `Started`인 호출을 결과와 함께 완료한다(후속 예약과 한 커밋으로 쓰는 `finish_with_wake`가 쓴다). 현재 claim·활성 판본을 같이 본다.
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

    /// 이 run의 소유자(요청자).
    pub fn principal_of(&self, attempt: &Attempt) -> Result<PrincipalId, Error> {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(i.fence(attempt)?.context.principal.clone())
    }

    /// 활성 판본이 `checked`와 같은지 포함해 현재 claim을 확인한다(`UserSchedules::list`가 쓴다).
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
