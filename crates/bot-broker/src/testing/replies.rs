//! In-memory `ScheduledReplies` fake matching production store contracts for frozen drafts, active-set fences, atomic enqueue, and unsent settlement.

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

#[derive(Clone)]
struct Finished {
    generation: Generation,
    claim: Option<ClaimEpoch>,
    activation: Revision,
    receipt: JsonPayload,
    unsent: Option<UnsentReason>,
}

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

#[derive(Clone, Debug, Default)]
enum ActiveSet {
    /// Tracks whichever activation set froze the draft (default steady state).
    #[default]
    Follows,
    Is(Revision),
    /// Host is not in any active set: blocks sending and receipt reads.
    Absent,
}

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
    active_set: Mutex<ActiveSet>,
    finished: Mutex<HashMap<(ProfileId, RunId), Finished>>,
    failure: Mutex<Option<Error>>,
    lose_response: Mutex<bool>,
    unsent_failure: Mutex<Option<Error>>,
    notices: Mutex<Vec<(RunId, String)>>,
}

use super::lock;

impl MemReplies {
    fn decided(&self, run: &RunId) -> bool {
        self.ledger
            .approval_states_of(&run.0, DELIVER_CALL)
            .iter()
            .any(|state| matches!(state, ApprovalState::Denied | ApprovalState::Revoked))
    }

    /// Stored owner denial/revocation takes precedence over route-loss settlement.
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

    pub fn set_active_set(&self, set: &str) {
        *lock(&self.active_set) = ActiveSet::Is(Revision(set.into()));
    }

    pub fn clear_active_set(&self) {
        *lock(&self.active_set) = ActiveSet::Absent;
    }

    pub fn fail_with(&self, error: Option<Error>) {
        *lock(&self.failure) = error;
    }

    pub fn lose_response(&self, lose: bool) {
        *lock(&self.lose_response) = lose;
    }

    pub fn fail_unsent_with(&self, error: Option<Error>) {
        *lock(&self.unsent_failure) = error;
    }

    pub fn notices(&self) -> Vec<(RunId, String)> {
        lock(&self.notices).clone()
    }

    fn set_state(&self, frozen: &Revision) -> SetState {
        match &*lock(&self.active_set) {
            ActiveSet::Follows => SetState::Same,
            ActiveSet::Is(active) if active == frozen => SetState::Same,
            ActiveSet::Is(_) => SetState::Changed,
            ActiveSet::Absent => SetState::Absent,
        }
    }

    pub fn outbox(&self) -> Vec<Sent> {
        lock(&self.outbox).clone()
    }
}

impl ScheduledReplies for MemReplies {
    fn receipt(&self, attempt: Attempt) -> Reply<'_, Option<JsonPayload>> {
        Box::pin(async move {
            let key = (attempt.profile.clone(), attempt.run.clone());
            let Some(done) = lock(&self.finished).get(&key).cloned() else {
                return Ok(None);
            };
            let eligible = attempt.generation == done.generation && attempt.claim == done.claim;
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
            self.ledger.principal_of(&attempt)?;
            let Some(frozen) = lock(&self.drafts).get(&attempt.run).map(Frozen::draft) else {
                return Ok(None);
            };
            match self.set_state(&frozen.activation) {
                SetState::Same => Ok(Some(frozen)),
                SetState::Changed => Err(Error::Conflict),
                SetState::Absent => Err(Error::Stale),
            }
        })
    }

    fn target(&self, attempt: Attempt) -> Reply<'_, Option<DeliveryTarget>> {
        Box::pin(async move {
            self.ledger.principal_of(&attempt)?;
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
            let (profile, run, claim) = match &fence {
                DeliveryFence::Claim(attempt) => {
                    // Check idempotent replay first since a finished run can no longer pass claim checks.
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
                    let stored = self.ledger.approval_lookup(nonce).ok_or(Error::Missing)?;
                    let at = &stored.approval.attempt;
                    if stored.approval.approver != *actor
                        || !actor.is_human()
                        || stored.approval.call.0 != DELIVER_CALL
                        || stored.approval.tool.0 != DELIVER_TOOL
                    {
                        return Err(Error::Denied);
                    }
                    if !matches!(
                        stored.state,
                        ApprovalState::Pending | ApprovalState::Approved
                    ) {
                        return Err(Error::Stale);
                    }
                    self.ledger.identity_ok(at)?;
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
            if lock(&self.outbox).iter().any(|sent| sent.run == run) {
                return Err(Error::Conflict);
            }
            let activation = match &*lock(&self.active_set) {
                ActiveSet::Follows => frozen.activation.clone(),
                ActiveSet::Is(active) => active.clone(),
                ActiveSet::Absent => return Err(Error::Stale),
            };
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
                return Err(Error::Denied);
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
                return Err(Error::Conflict);
            }
            if !self.ledger.has_call(&run.0, DELIVER_CALL) {
                return Err(Error::Missing);
            }
            if frozen.sha256()? != self.ledger.call_hash(&run.0, DELIVER_CALL) {
                return Err(Error::Conflict);
            }
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
                    return Err(Error::Unavailable);
                }
            }
            Ok(())
        })
    }
}
