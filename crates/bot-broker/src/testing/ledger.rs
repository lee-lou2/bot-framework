//! `CallLedger`의 기준 구현(메모리). 실제 저장소가 지켜야 할 불변식을 그대로 적었다.
//! - 워커 연산(`context`·`begin`·`open_approval`·`start`·`finish`)은 `attempt`의 세대와 claim epoch가 모두 현재일 때만
//!   한다(아니면 `Stale`, claim이 없으면 `Denied`). 취소된 run도 `Stale`이다. 정상 재개(승인 대기 뒤)는 세대를 유지하고
//!   claim epoch만 올린다. 승인은 (프로필, run, 세대)에만 묶인다(claim은 저장하지 않는다).
//! - `start`는 `checked_revision`이 저장소의 활성 판본과 같을 때만 한다(아니면 `Stale`). 권한을 검사한 뒤 활성 판본이
//!   바뀌었다면 승인 소비와 시작이 같은 잠금에서 함께 거절된다. 활성 판본이 없는 프로필(삭제·철회)도 `Stale`이다.
//! - 승인 결정·`supersede`·`approval_context`는 신원 기준(세대·취소)만 본다. 지정된 승인자만 할 수 있다.
//! - `start`는 승인 소비와 `Started` 기록을 한 번에 한다. 소비한 승인은 다시 쓸 수 없다(`Conflict`).
//!   `Open`에서만 시작한다. 요청 트리 private 플래그가 켜졌는데 꺼진 채로 판정했으면 `Conflict`다.
//! - `open_approval`은 이전 대기·승인을 `Superseded`로 닫는다. `decide`는 대기 중인 승인을 정하고,
//!   승인한 뒤 실행 전의 거절은 철회(`Revoked`)다. 결정되면 기다리는 run을 깨운다(`woken`).
//! - `finish`: `Open`→`Blocked`(권한·승인 거절), `Started`(같은 세대 또는 옛 세대)→`Done`·`Released`(`Open`으로
//!   복귀)·`Held`·`Blocked`, 판정이 있는 `Held`→`Done`(영수증)·`Released`(미실행).
//! - 상태 전이마다 감사 한 줄을 같은 임계 구역에서 쌓는다(`audit_log`).

use crate::approval::same_binding;
use crate::sys::Clock;
use bot_core::{
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
    /// 켜면 `context`가 답하지 못한다(원장 장애).
    context_down: AtomicBool,
    /// `context`가 답하기 전에 기다리는 시간(느린 원장).
    context_delay: Mutex<std::time::Duration>,
    /// `context`의 응답을 처음 폴링할 때 스레드를 이만큼 붙잡는다(실행이 밀린 상황: 응답과 마감이 함께 준비된다).
    context_block: Mutex<std::time::Duration>,
    /// 켜면 `begin`이 도구·인자 해시가 달라도 기존 기록을 그대로 돌려준다(저장소가 걸러 주지 못하는 경우를 흉내 낸다).
    lenient_begin: AtomicBool,
    /// `step` 읽기에 끼어드는 갈고리(붙잡기·읽기가 끝나는 순간의 일).
    steps: steps::StepProbe,
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
    /// 성공으로 끝나 리스를 놓은 run(저장소가 run을 끝낸 커밋 뒤). 워커 연산은 `Stale`이다.
    ended: bool,
}

struct ApprovalEntry {
    stored: StoredApproval,
    key: (RunId, CallId),
}

impl Inner {
    /// 신원 기준 확인: 프로필·세대·취소. claim은 보지 않는다(승인·화면·추적용).
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

    /// 워커 연산 확인: 신원에 더해 이 attempt가 현재 claim을 쥐고 있어야 한다.
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
mod steps;

impl MemLedger {
    /// 판본·세대·승인 대조와 `Started` 기록(일반 `start`와 `complete_share`가 같이 쓴다).
    fn begin_run(i: &mut Inner, now: SystemTime, start: StartCall) -> Result<(), Error> {
        i.fence(&start.attempt)?;
        // 기대 판본과 명시적으로 같아야 한다. 활성 프로필이 없어도(삭제·철회) 통과시키지 않는다.
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
        };
        i.audit.push("call.start".into());
        Ok(())
    }

    /// 기억 공유의 저장소 쪽 커밋: 호출에 묶인 지문이 `expected`(지금 본문으로 다시 계산한 값)와 같을 때만 승인 소비·시작·완료를
    /// 한 번에 한다. 다르면 아무것도 바꾸지 않고 `Conflict`다(승인은 소비되지 않는다). 완료 결과에는 기억 ID만 남긴다.
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
        if record.phase_is_done() {
            return Ok(()); // 완료한 호출의 재시도는 다시 공유하지 않는다
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

impl MemLedger {
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
        if record.phase_is_done() {
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
                std::thread::sleep(block); // 폴링하는 동안 실행기를 붙잡는다
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
            i.approvals.insert(
                a.nonce.clone(),
                ApprovalEntry {
                    stored: StoredApproval {
                        // 승인에는 claim을 저장하지 않는다(세대 신원만).
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
            context.attempt.claim = None; // 신원만: 워커 연산을 인가하지 못한다
            context.private_memory_read = i.private_flag(&entry.stored.approval.attempt.run);
            Ok(context)
        })
    }

    fn approval(&self, nonce: ApprovalId) -> Reply<'_, StoredApproval> {
        self.with(|i, _| {
            i.approvals
                .get(&nonce)
                .map(|e| e.stored.clone())
                .ok_or(Error::Missing)
        })
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
            // memory.share는 승인과 공유를 함께 커밋하는 `MemoryStore::share`로만 시작한다. 예약 생성·취소와 예약 회신 전송도 저장소의
            // 전용 커밋(`UserSchedules`·`ScheduledReplies::enqueue`)으로만 한다.
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
}

trait PhaseIsDone {
    fn phase_is_done(&self) -> bool;
}

impl PhaseIsDone for CallRecord {
    fn phase_is_done(&self) -> bool {
        matches!(self.phase, CallPhase::Done(_))
    }
}
