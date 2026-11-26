//! `PrivateLedger` 가짜(메모리). 실제 저장소가 지켜야 할 불변식을 그대로 적었다.
//! - 접수 키마다 한 행: 지문·에이전트·도구·대화가 같아야 재점유한다(다르면 `Conflict`).
//! - 단계: `Reserved`(리스 점유, 미시작) → `Started`(`start` 한 번) → `Completed`/`Held`/`Rejected`.
//! - `RetryBeforeStart`는 미시작일 때만 리스를 반납한다(시작했으면 `Conflict`). `NotSent`는 시작한 행을 반납한다.
//!   반납된 행은 같은 입력의 재전달로 새 epoch를 받는다. 반납하지 않은 행의 재전달은 `InProgress`다.
//! - `start`는 현재 리스(attempt·claim)·도구·지문·활성 판본을 대조하고 한 번만 커밋한다.
//! - 시작한 행이 리스를 잃으면 `expire`로 내구 `Held`가 된다(재실행 없음).

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
    /// 리스를 쥐고 있다(미시작).
    Reserved,
    /// 반납됐다(재전달이 새 epoch로 다시 점유한다).
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

/// 저장한 것(원문 없음): 접수 키, 지문, 에이전트, 도구, 스코프.
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

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

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
    /// 새 접수에 줄 시도의 바탕(저장소가 만든 private run의 시도). claim epoch는 점유할 때마다 오른다.
    pub fn set_attempt(&self, attempt: Attempt) {
        *lock(&self.base) = Some(attempt);
    }
    /// 저장소의 활성 판본. `start`가 `checked_revision`과 명시적으로 대조한다(없으면 `Stale`).
    pub fn set_active_revision(&self, revision: &str) {
        *lock(&self.active) = Some(Revision(revision.into()));
    }
    pub fn stored(&self) -> Vec<Stored> {
        lock(&self.stored).clone()
    }
    /// 종결·반납 기록(고정 어휘).
    pub fn outcomes(&self) -> Vec<&'static str> {
        lock(&self.outcomes).clone()
    }
    /// 행마다 마지막 단계.
    pub fn phases(&self) -> Vec<String> {
        lock(&self.rows)
            .values()
            .map(|r| format!("{:?}", r.phase))
            .collect()
    }
    /// 성공한 `start` 기록.
    pub fn starts(&self) -> Vec<(ToolId, [u8; 32], Revision)> {
        lock(&self.starts).clone()
    }
    /// `start`가 판본을 대조하기 직전에 한 번씩 실행할 동작(전송 직전의 판본 교체를 흉내 낸다).
    pub fn set_before_start(&self, hook: impl Fn() + Send + Sync + 'static) {
        *lock(&self.before_start) = Some(std::sync::Arc::new(hook));
    }
    /// 시작한 행이 리스를 잃은 것처럼 내구 보류로 바꾼다(재전달 없이 sweep).
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
            // 기대 판본과 명시적으로 같아야 한다. 활성 판본을 모르면(삭제·미설정) 통과시키지 않는다.
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
