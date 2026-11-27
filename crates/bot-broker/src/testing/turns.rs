//! Test doubles for `TurnSink`, checkpoint-only `RunStore`, `DelegationStore`, and `Park`.

use super::lock;
#[cfg(test)]
use crate::front::observe::{Observed, TurnSink};
use crate::front::park::{Park, ParkReason};
#[cfg(test)]
use bot_core::cli::CliRefusal;
use bot_core::{
    command::*,
    id::*,
    intake::{RequestKey, RequestState, RunProgress},
    port::{DelegationStore, Error, Reply, RunStore},
    state::RunContext,
};
use std::sync::{Arc, Mutex};

#[cfg(test)]
#[derive(Default)]
pub struct RecordingTurns {
    seen: Mutex<Vec<(Attempt, Observed)>>,
    fail: Mutex<Option<Error>>,
    delay: Mutex<std::time::Duration>,
    entered: tokio::sync::Notify,
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    refusals: Mutex<Vec<(Attempt, CallId, CliRefusal)>>,
    refusal_failure: Mutex<Option<Error>>,
    /// Optional ledger mirror writing `cli-refusal-<call>` checkpoints as `CheckpointSink` does.
    mirror: Mutex<Option<Arc<super::MemLedger>>>,
    refusal_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    refusal_entered: tokio::sync::Notify,
}

#[cfg(test)]
impl RecordingTurns {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn fail_with(&self, error: Option<Error>) {
        *lock(&self.fail) = error;
    }
    pub async fn entered(&self) {
        self.entered.notified().await;
    }
    pub fn hold_next(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *lock(&self.gate) = Some(gate.clone());
        gate
    }
    pub fn set_delay(&self, delay: std::time::Duration) {
        *lock(&self.delay) = delay;
    }
    pub fn seen(&self) -> Vec<Observed> {
        lock(&self.seen).iter().map(|(_, o)| o.clone()).collect()
    }
    pub fn attempts(&self) -> Vec<Attempt> {
        lock(&self.seen).iter().map(|(a, _)| a.clone()).collect()
    }
    pub fn refusals(&self) -> Vec<(CallId, CliRefusal)> {
        lock(&self.refusals)
            .iter()
            .map(|(_, call, refusal)| (call.clone(), *refusal))
            .collect()
    }
    pub fn fail_refusals_with(&self, error: Option<Error>) {
        *lock(&self.refusal_failure) = error;
    }
    pub fn mirror_into(&self, ledger: Arc<super::MemLedger>) {
        *lock(&self.mirror) = Some(ledger);
    }
    pub fn hold_next_refusal(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *lock(&self.refusal_gate) = Some(gate.clone());
        gate
    }
    pub async fn refusal_entered(&self) {
        self.refusal_entered.notified().await;
    }
}

#[cfg(test)]
impl TurnSink for RecordingTurns {
    fn completed(&self, attempt: Attempt, turn: Observed) -> Reply<'_, ()> {
        let delay = *lock(&self.delay);
        let failure = match lock(&self.fail).as_ref() {
            Some(Error::Stale) => Some(Error::Stale),
            Some(Error::Denied) => Some(Error::Denied),
            Some(_) => Some(Error::Unavailable),
            None => None,
        };
        let gate = lock(&self.gate).take();
        Box::pin(async move {
            self.entered.notify_one();
            if let Some(gate) = gate {
                let _ = gate.acquire().await;
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            match failure {
                Some(error) => Err(error),
                None => {
                    lock(&self.seen).push((attempt, turn));
                    Ok(())
                }
            }
        })
    }

    fn refused(&self, attempt: Attempt, call: CallId, refusal: CliRefusal) -> Reply<'_, ()> {
        let failure = match lock(&self.refusal_failure).as_ref() {
            Some(Error::Stale) => Some(Error::Stale),
            Some(_) => Some(Error::Unavailable),
            None => None,
        };
        let gate = lock(&self.refusal_gate).take();
        let mirror = lock(&self.mirror).clone();
        Box::pin(async move {
            self.refusal_entered.notify_one();
            if let Some(gate) = gate {
                let _ = gate.acquire().await;
            }
            if let Some(error) = failure {
                return Err(error);
            }
            if let Some(ledger) = mirror {
                let key = CliRefusal::key(&call);
                let body = serde_json::to_string(&refusal).map_err(|_| Error::Invalid)?;
                let stored =
                    bot_core::ledger::CallLedger::step(&*ledger, attempt.clone(), key.clone())
                        .await?;
                match stored {
                    Some(old) if old.0 != body => return Err(Error::Conflict),
                    Some(_) => {}
                    None => ledger.add_step(&attempt.run.0, &key, &body),
                }
            }
            lock(&self.refusals).push((attempt, call, refusal));
            Ok(())
        })
    }
}

/// Checkpoint-only `RunStore` double recording written steps.
#[derive(Default)]
pub struct FakeRuns {
    steps: Mutex<Vec<(Attempt, Step)>>,
}

impl FakeRuns {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn steps(&self) -> Vec<(Attempt, Step)> {
        lock(&self.steps).clone()
    }
}

impl RunStore for FakeRuns {
    fn accept_with(&self, _: Accept, _: AcceptExtras) -> Reply<'_, RunId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn status(&self, _: RequestKey) -> Reply<'_, Option<RequestState>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn progress(&self, _: ProfileId, _: RunId) -> Reply<'_, Option<RunProgress>> {
        Box::pin(async { Ok(None) })
    }
    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn release_for_shutdown(&self, _: Attempt) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn renew(&self, _: Attempt) -> Reply<'_, Beat> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn checkpoint_batch(&self, attempt: Attempt, steps: Vec<Step>) -> Reply<'_, ()> {
        let mut stored = lock(&self.steps);
        // Reject the whole batch on any key/body conflict; identical rewrites are idempotent.
        let clash = steps.iter().any(|new| {
            stored
                .iter()
                .any(|(_, old)| old.key == new.key && old.body != new.body)
        });
        if !clash {
            for step in steps {
                if !stored.iter().any(|(_, old)| old.key == step.key) {
                    stored.push((attempt.clone(), step));
                }
            }
        }
        Box::pin(async move { if clash { Err(Error::Conflict) } else { Ok(()) } })
    }
    fn finish(&self, _: Attempt, _: Finish) -> Reply<'_, Completion> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn finish_with(&self, a: Attempt, done: Finish, _: RunOutcome) -> Reply<'_, Completion> {
        self.finish(a, done)
    }
    fn fail(&self, _: Attempt, _: Failure) -> Reply<'_, Retry> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn history(&self, _: Attempt, _: u32) -> Reply<'_, Vec<Turn>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

fn same_step(old: &Delegate, new: &Delegate) -> bool {
    old.parent.profile == new.parent.profile
        && old.parent.run == new.parent.run
        && old.step == new.step
}

#[derive(Default)]
pub struct FakeDelegation {
    commands: Mutex<Vec<(RunId, Delegate)>>,
    fail: Mutex<Option<Error>>,
    finished: Mutex<Vec<(ProfileId, RunId, String, JsonPayload)>>,
    active: Mutex<Option<Revision>>,
}

impl FakeDelegation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn commands(&self) -> Vec<Delegate> {
        lock(&self.commands)
            .iter()
            .map(|(_, d)| d.clone())
            .collect()
    }
    pub fn finish_child(&self, profile: &str, parent_run: &str, step: &str, body: &str) {
        lock(&self.finished).push((
            ProfileId(profile.into()),
            RunId(parent_run.into()),
            step.into(),
            JsonPayload(body.into()),
        ));
    }
    pub fn set_active(&self, revision: &str) {
        *lock(&self.active) = Some(Revision(revision.into()));
    }
    pub fn fail_with(&self, error: Option<Error>) {
        *lock(&self.fail) = error;
    }
}

impl DelegationStore for FakeDelegation {
    fn result(&self, command: Delegate) -> Reply<'_, Option<JsonPayload>> {
        let outcome = (|| {
            if let Some(active) = lock(&self.active).as_ref()
                && *active != command.checked_revision
            {
                return Err(Error::Stale);
            }
            let commands = lock(&self.commands);
            let Some((_, old)) = commands.iter().find(|(_, d)| same_step(d, &command)) else {
                return Ok(None);
            };
            if old.child != command.child || old.instruction != command.instruction {
                return Err(Error::Conflict);
            }
            if !old.scopes.is_subset(&command.scopes) {
                return Err(Error::Denied);
            }
            Ok(lock(&self.finished)
                .iter()
                .find(|(profile, run, step, _)| {
                    *profile == command.parent.profile
                        && *run == command.parent.run
                        && *step == command.step
                })
                .map(|(_, _, _, body)| body.clone()))
        })();
        Box::pin(async move { outcome })
    }

    fn delegate(&self, command: Delegate) -> Reply<'_, RunId> {
        let outcome = match lock(&self.fail).as_ref() {
            Some(Error::Stale) => Err(Error::Stale),
            Some(Error::Denied) => Err(Error::Denied),
            Some(Error::Conflict) => Err(Error::Conflict),
            Some(_) => Err(Error::Unavailable),
            None => {
                let mut commands = lock(&self.commands);
                let same = commands.iter().find(|(_, d)| same_step(d, &command));
                match same {
                    Some((child, old))
                        if old.child == command.child && old.instruction == command.instruction =>
                    {
                        Ok(child.clone())
                    }
                    Some(_) => Err(Error::Conflict),
                    None => {
                        let child = RunId(format!("child-{}", commands.len() + 1));
                        commands.push((child.clone(), command));
                        Ok(child)
                    }
                }
            }
        };
        Box::pin(async move { outcome })
    }
}

#[derive(Default)]
pub struct RecordingPark {
    seen: Mutex<Vec<(Attempt, ParkReason)>>,
}

impl RecordingPark {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn seen(&self) -> Vec<(Attempt, ParkReason)> {
        lock(&self.seen).clone()
    }
}

impl Park for RecordingPark {
    fn parked(&self, attempt: &Attempt, reason: ParkReason) {
        lock(&self.seen).push((attempt.clone(), reason));
    }
}

/// `RunStore` double that expands `cli-resp-*` checkpoints into `cli-use-*` and `cli-group-*` steps in `MemLedger`.
pub struct PlanningRuns {
    ledger: Arc<super::MemLedger>,
    next_turn: Mutex<std::collections::HashMap<RunId, u64>>,
}

impl PlanningRuns {
    pub fn new(ledger: Arc<super::MemLedger>) -> Self {
        Self {
            ledger,
            next_turn: Mutex::default(),
        }
    }
}

impl RunStore for PlanningRuns {
    fn accept_with(&self, _: Accept, _: AcceptExtras) -> Reply<'_, RunId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn status(&self, _: RequestKey) -> Reply<'_, Option<RequestState>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn progress(&self, _: ProfileId, _: RunId) -> Reply<'_, Option<RunProgress>> {
        Box::pin(async { Ok(None) })
    }
    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn release_for_shutdown(&self, _: Attempt) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn renew(&self, _: Attempt) -> Reply<'_, Beat> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn checkpoint_batch(&self, attempt: Attempt, steps: Vec<Step>) -> Reply<'_, ()> {
        use bot_core::cli::{
            CliGroup, CliResponse, CliUse, GROUP_PREFIX, RESPONSE_PREFIX, USE_PREFIX,
        };
        let run = attempt.run.0.clone();
        let mut result = Ok(());
        for step in steps {
            self.ledger.add_step(&run, &step.key, &step.body.0);
            if !step.key.starts_with(RESPONSE_PREFIX) {
                continue;
            }
            let Some(response) = serde_json::from_str::<CliResponse>(&step.body.0)
                .ok()
                .filter(CliResponse::is_valid)
            else {
                result = Err(Error::Invalid);
                continue;
            };
            let turn = {
                let mut next = lock(&self.next_turn);
                let at = next.entry(attempt.run.clone()).or_insert(1);
                let turn = *at;
                *at += response.uses.len() as u64;
                turn
            };
            for (pos, used) in response.uses.iter().enumerate() {
                let plan = CliUse {
                    seq: turn + pos as u64,
                    turn,
                    pos: pos as u32,
                    name: used.name.clone(),
                    input: used.input.clone(),
                };
                self.ledger.add_step(
                    &run,
                    &format!("{USE_PREFIX}{}", used.id),
                    &serde_json::to_string(&plan).expect("json"),
                );
            }
            let group = CliGroup {
                seq: turn,
                uses: response.uses.iter().map(|u| u.id.clone()).collect(),
            };
            self.ledger.add_step(
                &run,
                &format!("{GROUP_PREFIX}{turn}"),
                &serde_json::to_string(&group).expect("json"),
            );
        }
        Box::pin(async move { result })
    }
    fn finish(&self, _: Attempt, _: Finish) -> Reply<'_, Completion> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn finish_with(&self, a: Attempt, done: Finish, _: RunOutcome) -> Reply<'_, Completion> {
        self.finish(a, done)
    }
    fn fail(&self, _: Attempt, _: Failure) -> Reply<'_, Retry> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn history(&self, _: Attempt, _: u32) -> Reply<'_, Vec<bot_core::command::Turn>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}
