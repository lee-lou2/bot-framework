//! `ScheduledReplies` 가짜(메모리). 실제 저장소가 지켜야 할 규칙을 그대로 적었다.
//! - `draft`는 현재 claim을 쥔 시도만 그 run이 동결한 초안을 읽는다(없으면 `None`).
//! - `enqueue`는 호출 ID `schedule-deliver`·되돌릴 수 없는 위험 등급·승인이 있을 때만 하고, **저장된 초안의 `sha256()`이 호출에 묶인 지문과
//!   같고 활성 집합이 초안을 동결한 때와 같을 때만** 승인 소비·목적지 outbox 등록·호출 완료·**run 완료(리스를 놓는다)**를 한 번에 한다.
//!   다르면 아무것도 바꾸지 않고 `Conflict`다(승인은 소비되지 않는다). 끝난 run은 일반 원장 조회(`context`·`lookup`)가 `Stale`이다.
//! - `receipt`는 성공한 run의 **마지막 claim**에만 outbox 등록 영수증을 준다(프로필·run·세대·claim 일치, 지금 활성 집합이 등록 때와 같을 것).
//!   등록 전에는 `None`이고(실행 권한은 일반 claim 검사가 확인한다), 등록 뒤 다른 claim이나 바뀐 집합은 `Stale`이다. 초안 본문은 읽지 않는다.
//! - `draft(None)`은 지금 활성 집합이 초안을 동결한 집합과 다르면 본문을 내주지 않고 `Conflict`다(활성 집합이 없으면 `Stale`).
//! - `target`은 현재 claim만, 동결한 **수신자만** 돌려준다(옛 집합에서 동결했어도 본문 없이, 초안이 없으면 `None`).
//! - `finish_unsent`는 펜스(현재 claim, 또는 승인 대기 중에는 유효한 카드와 인증된 소유자)·저장한 수신자·등록 전 상태를 확인하고 발생 하나만 끝낸다:
//!   `Unrouted`는 원래 대화에 고정 안내 한 건·옛 카드 무효·호출 완료·run 완료·미발송 영수증을 한 번에, `Rejected`는 저장된 소유자 본인의 거절·철회 결정이
//!   있을 때만(안내 없음). 같은 종결의 재전달은 같은 영수증이고, 이미 등록한 답은 `Conflict`다.

use super::MemLedger;
use bot_core::{
    command::{Finish, JsonPayload},
    id::*,
    ledger::ApprovalState,
    ledger::StartCall,
    port::{Error, Reply},
    schedule::{
        DELIVER_CALL, DELIVER_REVISION, DELIVER_TOOL, DeliveryDraft, DeliveryFence, DeliveryTarget,
        ScheduledReplies, UNROUTED_NOTICE, UnsentReason,
    },
    state::Risk,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// 동결한 초안(`DeliveryDraft`는 복제할 수 없어 필드로 들고 있다).
struct Frozen {
    activation: Revision,
    target: DeliveryTarget,
    answers: Vec<JsonPayload>,
}

impl Frozen {
    fn draft(&self) -> DeliveryDraft {
        DeliveryDraft {
            activation: self.activation.clone(),
            target: self.target.clone(),
            answers: self.answers.clone(),
        }
    }
}

/// 전송을 커밋하며 끝난 run의 마지막 시도와 영수증.
#[derive(Clone)]
struct Finished {
    generation: Generation,
    claim: Option<ClaimEpoch>,
    /// 종결 당시의 호스트 집합.
    activation: Revision,
    receipt: JsonPayload,
    /// 미발송으로 끝났으면 그 이유(등록한 전송이면 `None`).
    unsent: Option<UnsentReason>,
}

/// 지금 활성 집합과 초안을 동결한 집합의 관계.
enum SetState {
    Same,
    Changed,
    Absent,
}

fn same_target(a: &DeliveryTarget, b: &DeliveryTarget) -> bool {
    a.binding == b.binding
        && a.conversation.kind == b.conversation.kind
        && a.conversation.id == b.conversation.id
        && a.conversation.thread == b.conversation.thread
}

/// 지금 활성인 호스트 집합(저장소가 보는 값). 초안을 동결한 때의 집합과 같아야 보낸다.
#[derive(Clone, Debug, Default)]
enum ActiveSet {
    /// 정한 적이 없다: 초안을 동결한 집합 그대로다(집합이 바뀌지 않은 평소).
    #[default]
    Follows,
    Is(Revision),
    /// 활성 집합이 없다(호스트가 집합에서 빠졌다). 보내지도 이전 영수증을 읽게 하지도 않는다.
    Absent,
}

/// outbox에 등록된 전송 한 건: 어느 run이 어디로 무엇을 보내는가.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sent {
    pub run: RunId,
    pub binding: String,
    pub conversation: String,
    pub answers: Vec<String>,
}

pub struct MemReplies {
    ledger: Arc<MemLedger>,
    drafts: Mutex<HashMap<RunId, Frozen>>,
    outbox: Mutex<Vec<Sent>>,
    /// 지금 활성인 호스트 집합.
    active_set: Mutex<ActiveSet>,
    /// 전송을 커밋하며 끝난 run(프로필·run별).
    finished: Mutex<HashMap<(ProfileId, RunId), Finished>>,
    /// `enqueue`가 이 오류로 실패한다(저장소 장애).
    failure: Mutex<Option<Error>>,
    /// 켜면 `enqueue`가 커밋한 뒤 응답을 잃는다(`Unavailable`).
    lose_response: Mutex<bool>,
    /// `finish_unsent`가 이 오류로 실패한다(저장소 장애).
    unsent_failure: Mutex<Option<Error>>,
    /// 원래 대화에 남긴 고정 안내(run, 문구).
    notices: Mutex<Vec<(RunId, String)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl MemReplies {
    /// 소유자가 이 run의 전송 카드를 이미 거절했거나 소비 전에 철회했는가(저장된 결정).
    fn decided(&self, run: &RunId) -> bool {
        self.ledger
            .approval_states_of(&run.0, DELIVER_CALL)
            .iter()
            .any(|state| matches!(state, ApprovalState::Denied | ApprovalState::Revoked))
    }

    /// 요청한 이유를 저장된 소유자 결정에 비춰 가른 결과: 결정이 있으면 `Rejected`, 없으면 요청 그대로.
    fn settled(&self, run: &RunId, reason: UnsentReason) -> UnsentReason {
        if self.decided(run) {
            UnsentReason::Rejected
        } else {
            reason
        }
    }

    pub fn new(ledger: Arc<MemLedger>) -> Self {
        Self {
            ledger,
            drafts: Mutex::default(),
            outbox: Mutex::default(),
            active_set: Mutex::default(),
            finished: Mutex::default(),
            failure: Mutex::new(None),
            lose_response: Mutex::new(false),
            unsent_failure: Mutex::new(None),
            notices: Mutex::default(),
        }
    }

    /// 이 run의 최종 회신을 동결한다(이미 있으면 바꾼다: 동결한 뒤 달라지는 경쟁을 시험할 때 쓴다).
    pub fn set_draft(&self, run: &str, draft: DeliveryDraft) {
        lock(&self.drafts).insert(
            RunId(run.into()),
            Frozen {
                activation: draft.activation,
                target: draft.target,
                answers: draft.answers,
            },
        );
    }

    /// 지금 활성인 호스트 집합을 정한다(초안을 동결한 집합과 다르면 `enqueue`가 `Conflict`, 전송 뒤 `receipt`가 `Stale`이다).
    pub fn set_active_set(&self, set: &str) {
        *lock(&self.active_set) = ActiveSet::Is(Revision(set.into()));
    }

    /// 활성 집합이 없다(호스트가 집합에서 빠졌다): 보내지 못하고, 이미 끝난 전송의 영수증도 읽지 못한다.
    pub fn clear_active_set(&self) {
        *lock(&self.active_set) = ActiveSet::Absent;
    }

    /// 이후 `enqueue`가 이 오류로 실패한다(`None`이면 다시 정상).
    pub fn fail_with(&self, error: Option<Error>) {
        *lock(&self.failure) = error;
    }

    /// 켜면 `enqueue`가 커밋한 뒤 응답을 잃는다.
    pub fn lose_response(&self, lose: bool) {
        *lock(&self.lose_response) = lose;
    }

    /// 이후 `finish_unsent`가 이 오류로 실패한다(`None`이면 다시 정상).
    pub fn fail_unsent_with(&self, error: Option<Error>) {
        *lock(&self.unsent_failure) = error;
    }

    /// 원래 대화에 남긴 고정 안내 전부(run, 문구).
    pub fn notices(&self) -> Vec<(RunId, String)> {
        lock(&self.notices).clone()
    }

    /// 지금 활성 집합이 초안을 동결한 집합(`frozen`)과 어떤 관계인가.
    fn set_state(&self, frozen: &Revision) -> SetState {
        match &*lock(&self.active_set) {
            ActiveSet::Follows => SetState::Same,
            ActiveSet::Is(active) if active == frozen => SetState::Same,
            ActiveSet::Is(_) => SetState::Changed,
            ActiveSet::Absent => SetState::Absent,
        }
    }

    /// outbox에 등록된 전송 전부(등록 순서).
    pub fn outbox(&self) -> Vec<Sent> {
        lock(&self.outbox).clone()
    }
}

impl ScheduledReplies for MemReplies {
    fn receipt(&self, attempt: Attempt) -> Reply<'_, Option<JsonPayload>> {
        Box::pin(async move {
            // 등록 전: 영수증이 없다(실행 권한은 호출한 쪽이 일반 claim 검사로 확인한다).
            let key = (attempt.profile.clone(), attempt.run.clone());
            let Some(done) = lock(&self.finished).get(&key).cloned() else {
                return Ok(None);
            };
            let eligible = attempt.generation == done.generation && attempt.claim == done.claim;
            // 지금 활성 집합이 등록 때와 같아야 한다(바뀌었거나 없으면 수신 경로가 달라졌을 수 있다).
            let same_set = match &*lock(&self.active_set) {
                ActiveSet::Follows => true,
                ActiveSet::Is(active) => *active == done.activation,
                ActiveSet::Absent => false,
            };
            if eligible && same_set {
                Ok(Some(done.receipt))
            } else {
                Err(Error::Stale)
            }
        })
    }

    fn draft(
        &self,
        attempt: Attempt,
        _completion: Option<Finish>,
    ) -> Reply<'_, Option<DeliveryDraft>> {
        Box::pin(async move {
            self.ledger.principal_of(&attempt)?; // 현재 claim을 쥔 시도만 읽는다
            let Some(frozen) = lock(&self.drafts).get(&attempt.run).map(Frozen::draft) else {
                return Ok(None);
            };
            match self.set_state(&frozen.activation) {
                SetState::Same => Ok(Some(frozen)),
                // 활성 집합이 바뀌어 옛 집합에서 동결한 초안이다: 본문을 내주지 않는다
                SetState::Changed => Err(Error::Conflict),
                SetState::Absent => Err(Error::Stale),
            }
        })
    }

    fn target(&self, attempt: Attempt) -> Reply<'_, Option<DeliveryTarget>> {
        Box::pin(async move {
            self.ledger.principal_of(&attempt)?; // 현재 claim만
            if matches!(&*lock(&self.active_set), ActiveSet::Absent) {
                return Err(Error::Stale);
            }
            Ok(lock(&self.drafts)
                .get(&attempt.run)
                .map(|frozen| frozen.target.clone()))
        })
    }

    fn finish_unsent(
        &self,
        fence: DeliveryFence,
        target: DeliveryTarget,
        reason: UnsentReason,
    ) -> Reply<'_, JsonPayload> {
        Box::pin(async move {
            if let Some(error) = lock(&self.unsent_failure).as_ref() {
                return Err(match error {
                    Error::Stale => Error::Stale,
                    Error::Denied => Error::Denied,
                    Error::Conflict => Error::Conflict,
                    _ => Error::Unavailable,
                });
            }
            // 펜스: 어느 발생(프로필·run)의 무엇을 끝내는가
            let (profile, run, claim) = match &fence {
                DeliveryFence::Claim(attempt) => {
                    // 같은 종결의 재전달은 같은 영수증이다(run이 끝나 claim 펜스는 더 못 쓰므로 먼저 본다)
                    let key = (attempt.profile.clone(), attempt.run.clone());
                    if let Some(done) = lock(&self.finished).get(&key).cloned() {
                        let effective = self.settled(&attempt.run, reason);
                        return match done.unsent {
                            Some(was)
                                if was == effective
                                    && done.generation == attempt.generation
                                    && done.claim == attempt.claim =>
                            {
                                Ok(done.receipt)
                            }
                            _ => Err(Error::Conflict),
                        };
                    }
                    self.ledger.principal_of(attempt)?;
                    (attempt.profile.clone(), attempt.run.clone(), attempt.claim)
                }
                DeliveryFence::Approval { nonce, actor } => {
                    // 승인 대기 중: 유효한 카드와 인증된 소유자(사람)가 펜스다. 쥔 claim은 없다
                    let stored = self.ledger.approval_lookup(nonce).ok_or(Error::Missing)?;
                    let at = &stored.approval.attempt;
                    if stored.approval.approver != *actor
                        || !actor.is_human()
                        || stored.approval.call.0 != DELIVER_CALL
                        || stored.approval.tool.0 != DELIVER_TOOL
                    {
                        return Err(Error::Denied);
                    }
                    // 살아 있는 카드(대기·승인됨)만 이 펜스로 끝낸다. 소유자가 이미 거절·철회한 카드는 낡은 버튼이다(그 발생은 이어 받은 시도가 거절로 끝낸다)
                    if !matches!(
                        stored.state,
                        ApprovalState::Pending | ApprovalState::Approved
                    ) {
                        return Err(Error::Stale);
                    }
                    self.ledger.identity_ok(at)?;
                    // 마지막 claim이 이 run의 최종 시도다(그 시도만 나중에 영수증을 읽는다)
                    let last = Some(self.ledger.claim_epoch(&at.run.0));
                    (at.profile.clone(), at.run.clone(), last)
                }
            };
            let frozen = lock(&self.drafts)
                .get(&run)
                .map(Frozen::draft)
                .ok_or(Error::Missing)?;
            if !same_target(&frozen.target, &target) {
                return Err(Error::Conflict);
            }
            // 이미 등록한 답은 취소할 수 없다
            if lock(&self.outbox).iter().any(|sent| sent.run == run) {
                return Err(Error::Conflict);
            }
            // 현재 집합에서 끝낸다(활성 집합이 없으면 어느 집합의 일인지 모른다)
            let activation = match &*lock(&self.active_set) {
                ActiveSet::Follows => frozen.activation.clone(),
                ActiveSet::Is(active) => active.clone(),
                ActiveSet::Absent => return Err(Error::Stale),
            };
            // 저장된 소유자 결정(거절 또는 소비 전 철회)이 있으면 요청한 이유와 무관하게 «거절»이다(기록된 결정이 등록부 상태보다 먼저다).
            // «거절»은 그 결정이 있을 때만 쓴다.
            let reason = self.settled(&run, reason);
            if reason == UnsentReason::Rejected && !self.decided(&run) {
                return Err(Error::Denied);
            }
            let receipt = reason.receipt();
            if reason == UnsentReason::Unrouted {
                lock(&self.notices).push((run.clone(), UNROUTED_NOTICE.to_owned()));
            }
            self.ledger.supersede_open_approvals(&run.0, DELIVER_CALL);
            self.ledger
                .finish_call_with(&run.0, DELIVER_CALL, &receipt.0);
            self.ledger.end_run(&run.0);
            let generation = match &fence {
                DeliveryFence::Claim(attempt) => attempt.generation,
                DeliveryFence::Approval { nonce, .. } => self
                    .ledger
                    .approval_lookup(nonce)
                    .map_or(Generation(0), |stored| stored.approval.attempt.generation),
            };
            lock(&self.finished).insert(
                (profile, run),
                Finished {
                    generation,
                    claim,
                    activation,
                    receipt: receipt.clone(),
                    unsent: Some(reason),
                },
            );
            Ok(receipt)
        })
    }

    fn enqueue(&self, start: StartCall) -> Reply<'_, ()> {
        Box::pin(async move {
            if let Some(error) = lock(&self.failure).as_ref() {
                return Err(match error {
                    Error::Stale => Error::Stale,
                    Error::Denied => Error::Denied,
                    Error::Conflict => Error::Conflict,
                    _ => Error::Unavailable,
                });
            }
            if start.call.0 != DELIVER_CALL
                || start.tool_revision.0 != DELIVER_REVISION
                || start.risk != Risk::Irreversible
            {
                return Err(Error::Invalid);
            }
            if start.approval.is_none() {
                return Err(Error::Denied); // 승인 없이는 보내지 않는다
            }
            self.ledger.principal_of(&start.attempt)?;
            let run = start.attempt.run.clone();
            let frozen = lock(&self.drafts)
                .get(&run)
                .map(Frozen::draft)
                .ok_or(Error::Missing)?;
            let same_set = match &*lock(&self.active_set) {
                ActiveSet::Follows => true,
                ActiveSet::Is(active) => *active == frozen.activation,
                ActiveSet::Absent => false,
            };
            if !same_set {
                return Err(Error::Conflict); // 수신 경로가 달라졌을 수 있다
            }
            if !self.ledger.has_call(&run.0, DELIVER_CALL) {
                return Err(Error::Missing);
            }
            // 지금 저장된 초안으로 지문을 다시 계산해 호출에 묶인 지문(승인한 문서)과 같을 때만 보낸다.
            if frozen.sha256()? != self.ledger.call_hash(&run.0, DELIVER_CALL) {
                return Err(Error::Conflict);
            }
            // 새 커밋에서 영수증을 한 번 만든다(core가 정한 `{"queued":N}`). 호출 완료와 재조회가 같은 바이트를 쓴다.
            let receipt = frozen.queued_receipt();
            let (profile, generation, claim) = (
                start.attempt.profile.clone(),
                start.attempt.generation,
                start.attempt.claim,
            );
            if self
                .ledger
                .complete_builtin(start, DELIVER_TOOL, &receipt.0)?
            {
                // 같은 커밋에서 run이 성공으로 끝난다: 리스를 놓아 이 run의 일반 원장 조회는 `Stale`이 된다.
                self.ledger.end_run(&run.0);
                lock(&self.finished).insert(
                    (profile, run.clone()),
                    Finished {
                        generation,
                        claim,
                        activation: frozen.activation.clone(),
                        receipt,
                        unsent: None,
                    },
                );
                lock(&self.outbox).push(Sent {
                    run,
                    binding: frozen.target.binding.0.clone(),
                    conversation: frozen.target.conversation.id.clone(),
                    answers: frozen.answers.iter().map(|a| a.0.clone()).collect(),
                });
                if *lock(&self.lose_response) {
                    return Err(Error::Unavailable); // 커밋했지만 응답을 잃었다
                }
            }
            Ok(())
        })
    }
}
