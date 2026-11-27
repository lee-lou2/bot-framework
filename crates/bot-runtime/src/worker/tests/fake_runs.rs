//! Test double `RunStore` recording claims, heartbeats, checkpoints, completions, and failures.

use super::fakes::lock;
use bot_core::{
    command::*,
    id::{Attempt, ProfileId, RunId},
    intake::{RequestKey, RequestState, RunProgress},
    port::{Error, Reply, RunStore},
    state::RunContext,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub(super) struct RunsState {
    pub queue: VecDeque<RunContext>,
    pub history: Vec<Turn>,
    pub steps: Vec<Step>,
    pub finished: Vec<Finish>,
    pub outcomes: Vec<RunOutcome>,
    pub failed: Vec<Failure>,
    pub renews: u32,
    pub renew_script: VecDeque<Result<Beat, Error>>,
    pub finish_error: Option<Error>,
    pub history_error: Option<Error>,
    /// Mirrors real store fencing: reports cancel on heartbeat and rejects new reads with `Stale`.
    pub cancel: bool,
    pub events: Vec<String>,
}

pub(super) struct FakeRuns {
    pub state: Mutex<RunsState>,
}

impl FakeRuns {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(RunsState::default()),
        })
    }
    pub(super) fn queue(&self, ctx: RunContext) {
        lock(&self.state).queue.push_back(ctx);
    }
    pub(super) fn renew_script(&self, result: Result<Beat, Error>) {
        lock(&self.state).renew_script.push_back(result);
    }
    pub(super) fn events(&self) -> Vec<String> {
        lock(&self.state).events.clone()
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
        let next = lock(&self.state).queue.pop_front();
        Box::pin(async move { Ok(next) })
    }
    fn release_for_shutdown(&self, _: Attempt) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn renew(&self, _: Attempt) -> Reply<'_, Beat> {
        let mut state = lock(&self.state);
        state.renews += 1;
        let cancel_requested = state.cancel;
        let result = state
            .renew_script
            .pop_front()
            .unwrap_or(Ok(Beat { cancel_requested }));
        Box::pin(async move { result })
    }
    fn checkpoint_batch(&self, _: Attempt, steps: Vec<Step>) -> Reply<'_, ()> {
        let mut state = lock(&self.state);
        let mut pending = state.steps.clone();
        for step in steps {
            state.events.push(format!("checkpoint:{}", step.key));
            match pending.iter().find(|old| old.key == step.key) {
                Some(old) if old.body != step.body => {
                    return Box::pin(async { Err(Error::Conflict) });
                }
                Some(_) => {}
                None => pending.push(step),
            }
        }
        state.steps = pending;
        Box::pin(async { Ok(()) })
    }
    fn finish(&self, _: Attempt, done: Finish) -> Reply<'_, Completion> {
        let mut state = lock(&self.state);
        if state.cancel {
            return Box::pin(async { Ok(Completion::Cancelled) });
        }
        let error = state.finish_error.take();
        if error.is_none() {
            state.finished.push(done);
        }
        Box::pin(async move { error.map_or(Ok(Completion::Succeeded), Err) })
    }
    fn finish_with(&self, at: Attempt, done: Finish, outcome: RunOutcome) -> Reply<'_, Completion> {
        lock(&self.state).outcomes.push(outcome);
        self.finish(at, done)
    }
    fn fail(&self, _: Attempt, failure: Failure) -> Reply<'_, Retry> {
        lock(&self.state).failed.push(failure);
        Box::pin(async { Ok(Retry::Dead) })
    }
    fn history(&self, _: Attempt, _: u32) -> Reply<'_, Vec<Turn>> {
        let mut state = lock(&self.state);
        let result = match state.history_error.take() {
            Some(error) => Err(error),
            None if state.cancel => Err(Error::Stale),
            None => Ok(state.history.clone()),
        };
        Box::pin(async move { result })
    }
}
