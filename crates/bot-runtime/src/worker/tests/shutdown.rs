//! 종료 반납(워커): 종료 신호 뒤 유예가 지나도 남은 run은 실행기를 멈추고 정리(`Quiesce`)를 확인한 **뒤에만** `release_for_shutdown`으로 대기열에 돌아간다.
//! 확인하지 못했거나 확인 입구가 없으면 반납하지 않는다. 유예 안에 끝난 run·취소 요청이 선 run은 반납하지 않는다.

use super::{fake_runs::FakeRuns, fakes::*};
use crate::{
    host::{Deps, Quiesce, SingleModel, WorkerConfig},
    model::{ModelError, ModelRequest, ModelRunner, RunnerKind, StepFuture},
    worker::{Outcome, Worker},
};
use bot_core::{
    command::*,
    id::*,
    intake::{RequestKey, RequestState},
    port::{Broker, Error, Reply, RunStore},
    state::RunContext,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

/// `FakeRuns`에 `release_for_shutdown` 기록과 거절을 더한 저장소.
struct Runs {
    inner: Arc<FakeRuns>,
    released: Mutex<Vec<Attempt>>,
    refuse: Mutex<Option<Error>>,
}

impl RunStore for Runs {
    fn accept_with(&self, command: Accept, extras: AcceptExtras) -> Reply<'_, RunId> {
        self.inner.accept_with(command, extras)
    }
    fn status(&self, key: RequestKey) -> Reply<'_, Option<RequestState>> {
        self.inner.status(key)
    }
    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        self.inner.claim()
    }
    fn release_for_shutdown(&self, attempt: Attempt) -> Reply<'_, ()> {
        lock(&self.inner.state)
            .events
            .push(format!("release:{}", attempt.run.0));
        lock(&self.released).push(attempt);
        let refused = lock(&self.refuse).take();
        Box::pin(async move { refused.map_or(Ok(()), Err) })
    }
    fn renew(&self, attempt: Attempt) -> Reply<'_, Beat> {
        self.inner.renew(attempt)
    }
    fn checkpoint_batch(&self, attempt: Attempt, steps: Vec<Step>) -> Reply<'_, ()> {
        self.inner.checkpoint_batch(attempt, steps)
    }
    fn finish(&self, attempt: Attempt, done: Finish) -> Reply<'_, Completion> {
        self.inner.finish(attempt, done)
    }
    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry> {
        self.inner.fail(attempt, failure)
    }
    fn history(&self, attempt: Attempt, limit: u32) -> Reply<'_, Vec<Turn>> {
        self.inner.history(attempt, limit)
    }
}

/// 정리 확인 입구의 가짜: 부른 순서를 `events`에 남기고 정해 둔 결과를 낸다.
struct Quiet {
    runs: Arc<FakeRuns>,
    result: Mutex<Option<Error>>,
}

impl Quiesce for Quiet {
    fn quiesce(&self, attempt: Attempt) -> Reply<'_, ()> {
        lock(&self.runs.state)
            .events
            .push(format!("quiesce:{}", attempt.run.0));
        let result = lock(&self.result).take();
        Box::pin(async move { result.map_or(Ok(()), Err) })
    }
}

/// 취소 신호가 올 때까지 기다리는 CLI 모양 실행기(멈춘 순간을 `events`에 남긴다).
struct Hanging {
    runs: Arc<FakeRuns>,
}

impl ModelRunner for Hanging {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, mut request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            lock(&self.runs.state).events.push("model_started".into());
            while !*request.cancel.borrow() {
                if request.cancel.changed().await.is_err() {
                    break;
                }
            }
            lock(&self.runs.state).events.push("model_stopped".into());
            Err(ModelError::Cancelled)
        })
    }
}

/// 중단 신호를 받고도 잠시 뒤에야 돌아오는 실행기(취소 요청과 반납 신호가 모두 선 뒤에 `Cancelled`를 낸다).
struct SlowToStop;

impl ModelRunner for SlowToStop {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, _: ModelRequest) -> StepFuture<'_> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Err(ModelError::Cancelled)
        })
    }
}

/// 한참 걸려서 도구 호출 하나를 계획하는 Step 실행기(그 사이 취소 요청과 반납 신호가 모두 선다).
struct SlowPlan;

impl ModelRunner for SlowPlan {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }
    fn step(&self, _: ModelRequest) -> StepFuture<'_> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            calls("해볼게요", &[("r1:0:0", "agenda.list", "{}")])
        })
    }
}

/// 부르면 끝나지 않는 브로커(오래 걸리는 도구 호출).
struct Stuck {
    runs: Arc<FakeRuns>,
}

impl Broker for Stuck {
    fn call(&self, call: ToolCall) -> Reply<'_, CallResult> {
        lock(&self.runs.state)
            .events
            .push(format!("call_started:{}", call.tool.0));
        Box::pin(std::future::pending())
    }
    fn call_for_model(&self, call: ToolCall) -> Reply<'_, bot_core::port::ModelCall> {
        Box::pin(async move {
            self.call(call)
                .await
                .map(bot_core::port::ModelCall::Returned)
        })
    }
    fn decide(&self, _: ApprovalId, _: PrincipalId, _: bool) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

fn cfg() -> WorkerConfig {
    WorkerConfig {
        grace: Duration::from_millis(100),
        cleanup: Duration::from_secs(2),
        ..config()
    }
}

struct Setup {
    rig: Rig,
    runs: Arc<Runs>,
    worker: Worker,
}

/// 실행기 `model`(공유 기록 `events`를 받는다)로 도는 워커. `quiesce`가 `Some`이면 그 결과를 내는 확인 입구를 붙이고, `stuck`이면 도구 호출이 끝나지 않는다.
fn setup(
    model: impl FnOnce(&Arc<FakeRuns>) -> Arc<dyn ModelRunner>,
    quiesce: Option<Option<Error>>,
    stuck: bool,
) -> Setup {
    let rig = rig(ScriptedModel::new(vec![]));
    let runs = Arc::new(Runs {
        inner: rig.runs.clone(),
        released: Mutex::new(Vec::new()),
        refuse: Mutex::new(None),
    });
    let mut deps: Deps = (*rig.worker.deps).clone();
    deps.runs = runs.clone();
    deps.models = Arc::new(SingleModel(model(&rig.runs)));
    if stuck {
        deps.broker = Arc::new(Stuck {
            runs: rig.runs.clone(),
        });
    }
    let mut worker = Worker::new(deps, cfg());
    if let Some(result) = quiesce {
        worker = worker.with_quiesce(Arc::new(Quiet {
            runs: rig.runs.clone(),
            result: Mutex::new(result),
        }));
    }
    Setup { rig, runs, worker }
}

fn hanging(runs: &Arc<FakeRuns>) -> Arc<dyn ModelRunner> {
    Arc::new(Hanging { runs: runs.clone() })
}

/// 워커를 돌리다 `after` 뒤에 종료 신호를 보내고 `run`이 끝나기를 기다린다.
async fn serve_then_stop(worker: &Worker, after: Duration) {
    let (stop, shutdown) = watch::channel(false);
    let serving = {
        let worker = worker.clone();
        tokio::spawn(async move { worker.run(shutdown).await })
    };
    tokio::time::sleep(after).await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("종료 신호 뒤 워커가 끝난다")
        .unwrap();
}

fn position(events: &[String], what: &str) -> Option<usize> {
    events.iter().position(|e| e == what)
}

fn nothing_was_reported(s: &Setup) {
    let state = lock(&s.rig.runs.state);
    assert!(state.finished.is_empty(), "결과를 쓰지 않았다");
    assert!(state.failed.is_empty(), "실패를 보고하지 않았다");
}

#[tokio::test]
async fn a_run_still_going_after_the_grace_is_stopped_confirmed_and_returned_in_that_order() {
    let s = setup(hanging, Some(None), false);
    s.rig.runs.queue(agent_run("r1"));
    serve_then_stop(&s.worker, Duration::from_millis(80)).await;

    let events = s.rig.runs.events();
    let stopped = position(&events, "model_stopped").expect("모델을 멈췄다");
    let confirmed = position(&events, "quiesce:r1").expect("정리를 확인했다");
    let released = position(&events, "release:r1").expect("대기열로 돌려줬다");
    assert!(
        stopped < confirmed && confirmed < released,
        "프로세스를 멈추고 → 종료·세션 회수를 확인하고 → 반납한다: {events:?}"
    );
    assert_eq!(*lock(&s.runs.released), [agent_run("r1").attempt]);
    nothing_was_reported(&s);
}

#[tokio::test]
async fn an_unconfirmed_cleanup_releases_nothing_and_leaves_the_run_to_the_lease_handover() {
    let s = setup(hanging, Some(Some(Error::Unavailable)), false);
    s.rig.runs.queue(agent_run("r1"));
    serve_then_stop(&s.worker, Duration::from_millis(80)).await;

    let events = s.rig.runs.events();
    assert!(position(&events, "quiesce:r1").is_some());
    assert!(
        position(&events, "release:r1").is_none(),
        "확인하지 못했으니 반납하지 않는다: {events:?}"
    );
    nothing_was_reported(&s);
}

#[tokio::test]
async fn without_a_cleanup_port_nothing_is_released() {
    let s = setup(hanging, None, false);
    s.rig.runs.queue(agent_run("r1"));
    serve_then_stop(&s.worker, Duration::from_millis(80)).await;

    let events = s.rig.runs.events();
    assert!(position(&events, "model_stopped").is_some());
    assert!(position(&events, "release:r1").is_none(), "{events:?}");
    nothing_was_reported(&s);
}

#[tokio::test]
async fn a_refused_release_reports_nothing_either() {
    let s = setup(hanging, Some(None), false);
    *lock(&s.runs.refuse) = Some(Error::Stale);
    s.rig.runs.queue(agent_run("r1"));
    serve_then_stop(&s.worker, Duration::from_millis(80)).await;

    let events = s.rig.runs.events();
    assert!(position(&events, "release:r1").is_some());
    nothing_was_reported(&s);
}

#[tokio::test]
async fn a_run_that_finishes_within_the_grace_is_neither_confirmed_nor_released() {
    let s = setup(
        |_| ScriptedModel::new(vec![say("됐어요")]),
        Some(None),
        false,
    );
    s.rig.runs.queue(agent_run("r1"));
    serve_then_stop(&s.worker, Duration::from_millis(80)).await;

    assert_eq!(lock(&s.rig.runs.state).finished.len(), 1, "정상 완료했다");
    let events = s.rig.runs.events();
    assert!(position(&events, "quiesce:r1").is_none(), "{events:?}");
    assert!(position(&events, "release:r1").is_none(), "{events:?}");
}

#[tokio::test]
async fn a_tool_call_in_flight_is_dropped_by_the_release_signal_and_the_run_is_returned() {
    let s = setup(
        |_| ScriptedModel::new(vec![calls("해볼게요", &[("r1:0:0", "agenda.list", "{}")])]),
        Some(None),
        true,
    );
    s.rig.runs.queue(agent_run("r1"));
    serve_then_stop(&s.worker, Duration::from_millis(80)).await;

    let events = s.rig.runs.events();
    let started = position(&events, "call_started:agenda.list").expect("도구 호출이 시작됐다");
    let confirmed = position(&events, "quiesce:r1").expect("정리를 확인했다");
    let released = position(&events, "release:r1").expect("대기열로 돌려줬다");
    assert!(started < confirmed && confirmed < released, "{events:?}");
    nothing_was_reported(&s);
}

#[tokio::test]
async fn a_cancel_request_wins_over_the_release_signal() {
    let s = setup(|_| Arc::new(SlowToStop), Some(None), false);
    // 하트비트가 취소 요청을 알렸고(모든 갱신이 취소를 돌려준다) 실행기가 아직 돌아오기 전에 반납 신호도 왔다
    for _ in 0..200 {
        s.rig.runs.renew_script(Ok(Beat {
            cancel_requested: true,
        }));
    }
    let (release, rx) = watch::channel(false);
    let task = {
        let worker = s.worker.clone();
        tokio::spawn(async move { worker.execute_with(agent_run("r1"), rx).await })
    };
    tokio::time::sleep(Duration::from_millis(80)).await;
    release.send(true).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Outcome::Cancelled, "취소가 먼저다");
    let events = s.rig.runs.events();
    assert!(position(&events, "release:r1").is_none(), "{events:?}");
}

#[tokio::test]
async fn a_cancel_request_also_wins_at_the_checks_between_the_steps_of_a_run() {
    // 실행기가 돌아온 뒤 도구 호출 앞의 중단 확인에서 두 신호가 모두 서 있다: 취소가 먼저다(도구도 시작하지 않는다)
    let s = setup(|_| Arc::new(SlowPlan), Some(None), false);
    for _ in 0..200 {
        s.rig.runs.renew_script(Ok(Beat {
            cancel_requested: true,
        }));
    }
    let (release, rx) = watch::channel(false);
    let task = {
        let worker = s.worker.clone();
        tokio::spawn(async move { worker.execute_with(agent_run("r1"), rx).await })
    };
    tokio::time::sleep(Duration::from_millis(80)).await;
    release.send(true).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Outcome::Cancelled, "취소가 먼저다");
    let events = s.rig.runs.events();
    assert!(position(&events, "release:r1").is_none(), "{events:?}");
    assert!(position(&events, "quiesce:r1").is_none(), "{events:?}");
}
