//! In-memory `EffectJournal` test double issuing `send=true` once per `(run, call, step)`.

use bot_core::{
    command::*,
    id::*,
    port::{EffectJournal, EffectPermit, Error, Reply},
    state::{EffectResolution, EffectState},
};
use std::{collections::HashMap, sync::Mutex};

struct Step {
    effect: EffectId,
    args: String,
    state: EffectState,
    receipt: Option<String>,
    /// Operator verdict proving non-execution; consumed on the next successful claim.
    resolution: Option<EffectResolution>,
}

#[derive(Default)]
struct State {
    generations: HashMap<RunId, Generation>,
    /// Tool bound to a call; when set, `lookup` checks it matches.
    tools: HashMap<(RunId, CallId), ToolId>,
    /// When set, `lookup` returns `Error::Unavailable`.
    lookup_down: bool,
    /// When set, `lookup` injects bogus resolution/receipt fields to test broker filtering.
    lying: bool,
    steps: HashMap<(RunId, CallId, String), Step>,
    counter: u32,
}

#[derive(Default)]
pub struct MemJournal {
    inner: Mutex<State>,
}

fn status_of(step: &Step) -> EffectStatus {
    EffectStatus {
        effect: step.effect.clone(),
        state: step.state,
        resolution: step.resolution.filter(|_| step.state == EffectState::Held),
        receipt: step.receipt.clone(),
    }
}

impl MemJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the current generation for `run` (unset runs skip generation fencing).
    pub fn set_generation(&self, run: &str, generation: u64) {
        lock(&self.inner)
            .generations
            .insert(RunId(run.into()), Generation(generation));
    }

    /// Marks a held step as `NotExecuted`; returns `false` if not held.
    pub fn resolve_not_executed(&self, run: &str, call: &str, step: &str) -> bool {
        let mut state = lock(&self.inner);
        let key = (RunId(run.into()), CallId(call.into()), step.to_owned());
        match state.steps.get_mut(&key) {
            Some(found) if found.state == EffectState::Held => {
                found.resolution = Some(EffectResolution::NotExecuted);
                true
            }
            _ => false,
        }
    }

    /// Binds `tool` to `(run, call)` so `lookup` denies mismatches.
    pub fn bind_call(&self, run: &str, call: &str, tool: &str) {
        lock(&self.inner).tools.insert(
            (RunId(run.into()), CallId(call.into())),
            ToolId(tool.into()),
        );
    }

    /// Injects protocol-violating resolution and receipt fields into `lookup` responses.
    pub fn lying(&self, on: bool) {
        lock(&self.inner).lying = on;
    }

    /// Forces `lookup` to return `Error::Unavailable`.
    pub fn lookup_down(&self, down: bool) {
        lock(&self.inner).lookup_down = down;
    }

    /// Total recorded steps (verifies lookups do not create steps).
    pub fn step_count(&self) -> usize {
        lock(&self.inner).steps.len()
    }

    pub fn states(&self) -> Vec<(String, EffectState, Option<String>)> {
        let state = lock(&self.inner);
        let mut rows: Vec<_> = state
            .steps
            .iter()
            .map(|((_, _, step), s)| (step.clone(), s.state, s.receipt.clone()))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }
}

use super::lock;

fn fence(state: &State, attempt: &Attempt) -> Result<(), Error> {
    match state.generations.get(&attempt.run) {
        Some(current) if *current != attempt.generation => Err(Error::Stale),
        _ => Ok(()),
    }
}

impl EffectJournal for MemJournal {
    fn request(&self, request: EffectRequest) -> Reply<'_, EffectPermit> {
        let result = (|| {
            let mut state = lock(&self.inner);
            fence(&state, &request.attempt)?;
            let key = (
                request.attempt.run.clone(),
                request.call.clone(),
                request.step.clone(),
            );
            if let Some(step) = state.steps.get_mut(&key) {
                if step.args != request.args.0 {
                    return Err(Error::Conflict);
                }
                // Operator-resolved held step may be claimed once (`send=true`), consuming the verdict.
                if step.state == EffectState::Held
                    && step.resolution == Some(EffectResolution::NotExecuted)
                {
                    step.state = EffectState::Started;
                    step.receipt = None;
                    step.resolution = None;
                    return Ok(EffectPermit {
                        status: status_of(step),
                        send: true,
                    });
                }
                // Started steps transition to Held on re-request; never re-send.
                if matches!(step.state, EffectState::Started) {
                    step.state = EffectState::Held;
                    step.receipt = Some("response_unknown".into());
                }
                return Ok(EffectPermit {
                    status: status_of(step),
                    send: false,
                });
            }
            state.counter += 1;
            let step = Step {
                effect: EffectId(format!("e-{}", state.counter)),
                args: request.args.0,
                state: EffectState::Started,
                receipt: None,
                resolution: None,
            };
            let permit = EffectPermit {
                status: status_of(&step),
                send: true,
            };
            state.steps.insert(key, step);
            Ok(permit)
        })();
        Box::pin(async move { result })
    }

    /// Read-only lookup by `(run, call, step, args)`; never mutates state or grants `send`.
    fn lookup(&self, request: EffectRequest, tool: ToolId) -> Reply<'_, Option<EffectStatus>> {
        let result = (|| {
            let state = lock(&self.inner);
            if state.lookup_down {
                return Err(Error::Unavailable);
            }
            fence(&state, &request.attempt)?;
            if let Some(bound) = state
                .tools
                .get(&(request.attempt.run.clone(), request.call.clone()))
                && *bound != tool
            {
                return Err(Error::Denied);
            }
            let key = (
                request.attempt.run.clone(),
                request.call.clone(),
                request.step.clone(),
            );
            match state.steps.get(&key) {
                None => Ok(None),
                Some(step) if step.args != request.args.0 => Err(Error::Conflict),
                Some(step) => {
                    let mut status = status_of(step);
                    if state.lying {
                        status.resolution = Some(EffectResolution::NotExecuted);
                        status.receipt.get_or_insert_with(|| "stray".into());
                    }
                    Ok(Some(status))
                }
            }
        })();
        Box::pin(async move { result })
    }

    fn status(&self, attempt: Attempt, call: CallId, effect: EffectId) -> Reply<'_, EffectStatus> {
        let result = (|| {
            let state = lock(&self.inner);
            fence(&state, &attempt)?;
            state
                .steps
                .iter()
                .find(|((run, c, _), s)| *run == attempt.run && *c == call && s.effect == effect)
                .map(|(_, s)| status_of(s))
                .ok_or(Error::Missing)
        })();
        Box::pin(async move { result })
    }

    fn unresolved(&self, attempt: Attempt, call: CallId) -> Reply<'_, Vec<EffectStatus>> {
        let state = lock(&self.inner);
        let steps = state
            .steps
            .iter()
            .filter(|((run, c, _), s)| {
                *run == attempt.run
                    && *c == call
                    && matches!(s.state, EffectState::Started | EffectState::Held)
                    && s.resolution.is_none()
            })
            .map(|(_, s)| status_of(s))
            .collect();
        Box::pin(async move { Ok(steps) })
    }

    fn record(&self, attempt: Attempt, call: CallId, status: EffectStatus) -> Reply<'_, ()> {
        let result = (|| {
            let mut state = lock(&self.inner);
            fence(&state, &attempt)?;
            // Resolution is operator-only; tool record frames cannot set it.
            if status.resolution.is_some() {
                return Err(Error::Invalid);
            }
            let step = state
                .steps
                .iter_mut()
                .find(|((run, c, _), s)| {
                    *run == attempt.run && *c == call && s.effect == status.effect
                })
                .map(|(_, s)| s)
                .ok_or(Error::Denied)?;
            match (step.state, status.state) {
                (EffectState::Confirmed, EffectState::Confirmed)
                    if step.receipt == status.receipt =>
                {
                    Ok(())
                }
                (EffectState::Started | EffectState::Held, next)
                    if next != EffectState::Started =>
                {
                    step.state = next;
                    step.receipt = status.receipt;
                    step.resolution = None;
                    Ok(())
                }
                _ => Err(Error::Conflict),
            }
        })();
        Box::pin(async move { result })
    }
}
