//! 완료한 모델 응답의 도구 목록을 받는 가짜(`TurnSink`)와, 체크포인트만 되는 가짜 `RunStore`.

use crate::front::{
    observe::{Observed, TurnSink},
    park::{Park, ParkReason},
};
use bot_core::{
    cli::CliRefusal,
    command::*,
    id::*,
    intake::{RequestKey, RequestState},
    port::{DelegationStore, Error, Reply, RunStore},
    state::RunContext,
};
use std::sync::{Arc, Mutex};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 받은 응답을 기록한다. `fail_with`로 쓰기 실패를 흉내 낸다.
#[derive(Default)]
pub struct RecordingTurns {
    seen: Mutex<Vec<(Attempt, Observed)>>,
    fail: Mutex<Option<Error>>,
    /// 저장이 이만큼 걸린다(느린 저장소).
    delay: Mutex<std::time::Duration>,
    /// 저장에 들어설 때마다 알린다(시험이 시간 추정 없이 그 순간에 맞춰 움직이게).
    entered: tokio::sync::Notify,
    /// 다음 저장을 이 문이 열릴 때까지 붙잡아 둔다.
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// 남긴 확정 거절 표식(시도·호출 ID·종류).
    refusals: Mutex<Vec<(Attempt, CallId, CliRefusal)>>,
    /// 표식 쓰기가 이 오류로 실패한다.
    refusal_failure: Mutex<Option<Error>>,
    /// 표식을 실제 체크포인트처럼 이 원장의 run 체크포인트(`cli-refusal-<호출 ID>`)에도 쓴다(`CallBook::marked`가 읽는 곳).
    mirror: Mutex<Option<Arc<super::MemLedger>>>,
    /// 다음 표식 쓰기를 이 문이 열릴 때까지 붙잡아 둔다(동시 재전달 시험이 그 순간에 맞춰 움직이게).
    refusal_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// 표식 쓰기에 들어설 때마다 알린다.
    refusal_entered: tokio::sync::Notify,
}

impl RecordingTurns {
    pub fn new() -> Self {
        Self::default()
    }
    /// 이후 알림이 이 오류로 실패한다(`None`이면 다시 성공).
    pub fn fail_with(&self, error: Option<Error>) {
        *lock(&self.fail) = error;
    }
    /// 저장에 들어설 때까지 기다린다(이미 들어섰다면 바로 돌아온다).
    pub async fn entered(&self) {
        self.entered.notified().await;
    }
    /// 다음 저장을 붙잡아 둔다. 돌려준 문에 `add_permits(1)`을 하면 풀린다.
    pub fn hold_next(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *lock(&self.gate) = Some(gate.clone());
        gate
    }
    /// 이후 저장이 `delay`만큼 걸린다.
    pub fn set_delay(&self, delay: std::time::Duration) {
        *lock(&self.delay) = delay;
    }
    pub fn seen(&self) -> Vec<Observed> {
        lock(&self.seen).iter().map(|(_, o)| o.clone()).collect()
    }
    pub fn attempts(&self) -> Vec<Attempt> {
        lock(&self.seen).iter().map(|(a, _)| a.clone()).collect()
    }
    /// 남긴 확정 거절 표식(호출 ID와 종류).
    pub fn refusals(&self) -> Vec<(CallId, CliRefusal)> {
        lock(&self.refusals)
            .iter()
            .map(|(_, call, refusal)| (call.clone(), *refusal))
            .collect()
    }
    /// 이후 표식 쓰기가 이 오류로 실패한다(`None`이면 다시 성공).
    pub fn fail_refusals_with(&self, error: Option<Error>) {
        *lock(&self.refusal_failure) = error;
    }
    /// 표식을 이 원장의 체크포인트에도 쓴다(실제 `CheckpointSink`처럼: 같은 키에 다른 내용은 `Conflict`, claim을 잃으면 `Stale`).
    pub fn mirror_into(&self, ledger: Arc<super::MemLedger>) {
        *lock(&self.mirror) = Some(ledger);
    }
    /// 다음 표식 쓰기를 붙잡아 둔다. 돌려준 문에 `add_permits(1)`을 하면 풀린다.
    pub fn hold_next_refusal(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *lock(&self.refusal_gate) = Some(gate.clone());
        gate
    }
    /// 표식 쓰기에 들어설 때까지 기다린다.
    pub async fn refusal_entered(&self) {
        self.refusal_entered.notified().await;
    }
}

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
                // 체크포인트는 원장의 `step`/`add_step`으로 흉내 낸다(실제 `CheckpointSink`는 `RunStore::checkpoint`)
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

/// `checkpoint`만 되는 `RunStore`: 쓴 칸을 기록한다. 나머지는 쓰지 않는다.
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
    fn accept(&self, _: Accept) -> Reply<'_, RunId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn status(&self, _: RequestKey) -> Reply<'_, Option<RequestState>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn renew(&self, _: Attempt) -> Reply<'_, Beat> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn checkpoint_batch(&self, attempt: Attempt, steps: Vec<Step>) -> Reply<'_, ()> {
        let mut stored = lock(&self.steps);
        // 하나라도 기존 키와 내용이 다르면 전체를 받지 않는다(저장소와 같다). 같은 내용의 재전송은 멱등이다.
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
    fn finish(&self, _: Attempt, _: Finish) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn fail(&self, _: Attempt, _: Failure) -> Reply<'_, Retry> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn history(&self, _: Attempt, _: u32) -> Reply<'_, Vec<Turn>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

/// 같은 단계인가: 운영 저장소처럼 (프로필, 부모 run, 단계)가 같아야 한다.
fn same_step(old: &Delegate, new: &Delegate) -> bool {
    old.parent.profile == new.parent.profile
        && old.parent.run == new.parent.run
        && old.step == new.step
}

/// 위임 저장소 가짜: 받은 명령을 기록한다. (부모 run, 단계)로 유일하며, 같은 대상·지시의 재요청은 같은 자식이고 다르면 `Conflict`다.
#[derive(Default)]
pub struct FakeDelegation {
    commands: Mutex<Vec<(RunId, Delegate)>>,
    fail: Mutex<Option<Error>>,
    /// 끝난 자식의 결과(프로필, 부모 run, 단계) → 본문. 운영 저장소처럼 프로필까지 키다.
    finished: Mutex<Vec<(ProfileId, RunId, String, JsonPayload)>>,
    /// 저장소가 활성으로 보는 판본(`None`이면 대조하지 않는다).
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
    /// 이 (부모 run, 단계)의 자식이 끝났다고 기록한다. 본문은 체크포인트와 같은 모양
    /// (`{"state":"succeeded","result":…}` · `{"state":"dead"|"cancelled","error":코드}`)이다.
    pub fn finish_child(&self, profile: &str, parent_run: &str, step: &str, body: &str) {
        lock(&self.finished).push((
            ProfileId(profile.into()),
            RunId(parent_run.into()),
            step.into(),
            JsonPayload(body.into()),
        ));
    }
    /// 저장소가 이 판본을 활성으로 본다고 한다(`result`가 `checked_revision`과 다르면 `Stale`).
    pub fn set_active(&self, revision: &str) {
        *lock(&self.active) = Some(Revision(revision.into()));
    }
    /// 이후 위임이 이 오류로 실패한다(`None`이면 다시 성공).
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
                return Ok(None); // 자식이 없다
            };
            // 대상·지시가 처음 위임과 같아야 하고, 처음 넘긴 범위가 지금 계산한 범위 안에 있어야 한다.
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

/// 대기에 들어간 시도를 기록하는 `Park`.
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

/// ②의 세션 대조를 흉내 내는 `RunStore`: `cli-resp-<id>` 체크포인트가 오면 `MemLedger`의 이 run 체크포인트에 그대로 쓰고, 그 응답의
/// 도구마다 `cli-use-<id>`와 응답 전체의 `cli-group-<turn>`을 같은 묶음으로 더한다(`turn`은 run 안에서 1부터 이어지는 번호이고,
/// 도구의 `seq`는 `turn + 위치`다). 실제 CLI 시험이 `PlannedCalls`와 `CheckpointSink`를 그대로 쓰게 한다.
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
    fn accept(&self, _: Accept) -> Reply<'_, RunId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn status(&self, _: RequestKey) -> Reply<'_, Option<RequestState>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn claim(&self) -> Reply<'_, Option<RunContext>> {
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
    fn finish(&self, _: Attempt, _: Finish) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn fail(&self, _: Attempt, _: Failure) -> Reply<'_, Retry> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn history(&self, _: Attempt, _: u32) -> Reply<'_, Vec<bot_core::command::Turn>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}
