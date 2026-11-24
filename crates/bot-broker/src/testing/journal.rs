//! `EffectJournal` 가짜(메모리). 호출 ID + 단계 키로 한 번만 `send=true`를 준다.

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
    /// 운영자가 미실행을 입증한 보류 단계(다음 점유가 성공하면 소비된다).
    resolution: Option<EffectResolution>,
}

#[derive(Default)]
struct State {
    generations: HashMap<RunId, Generation>,
    /// 호출을 연 도구(정한 호출만 조회가 도구를 대조한다).
    tools: HashMap<(RunId, CallId), ToolId>,
    /// 켜면 조회가 원장을 읽지 못한다(`Unavailable`).
    lookup_down: bool,
    /// 켜면 조회가 규약을 어긴 답을 낸다(어느 상태에나 판정·영수증을 싣는다). 브로커가 그대로 옮기지 않는지 본다.
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

    /// 이 run의 현재 세대를 정한다. 정하지 않은 run은 세대 검사를 하지 않는다.
    pub fn set_generation(&self, run: &str, generation: u64) {
        lock(&self.inner)
            .generations
            .insert(RunId(run.into()), Generation(generation));
    }

    /// 운영자가 이 보류 단계의 미실행을 입증했다(보류가 아닌 단계는 받지 않는다: `false`).
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

    /// 이 호출을 연 도구를 정한다. 정한 호출의 조회는 도구가 다르면 `Denied`다(정하지 않은 호출은 대조하지 않는다).
    pub fn bind_call(&self, run: &str, call: &str, tool: &str) {
        lock(&self.inner).tools.insert(
            (RunId(run.into()), CallId(call.into())),
            ToolId(tool.into()),
        );
    }

    /// 켜면 이후 조회가 원장을 읽지 못한다.
    /// 조회가 규약을 어긴 답을 내게 한다(어느 상태에나 판정과 영수증을 싣는다).
    pub fn lying(&self, on: bool) {
        lock(&self.inner).lying = on;
    }

    pub fn lookup_down(&self, down: bool) {
        lock(&self.inner).lookup_down = down;
    }

    /// 지금까지 만든 단계 수(조회가 단계를 만들지 않는지 보려고).
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

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

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
                // 운영자가 미실행을 입증한 보류 단계는 같은 단계를 다시 점유한다: 한 번만 `send=true`이고 판정은 소비된다
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
                // 확정·거절은 그대로, 전송 여부가 불명인 시작은 보류다. 둘 다 다시 보내지 않는다.
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

    /// 조회 전용: 이 호출의 그 단계만, 같은 정규 인자 지문으로 읽는다. 아무것도 쓰지 않고(시작만 한 단계도 보류로 바꾸지 않는다) 전송을 허가하지 않는다.
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
            // 판정은 운영자만 정한다: 기록 경로로는 어떤 판정도 받지 않는다
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
