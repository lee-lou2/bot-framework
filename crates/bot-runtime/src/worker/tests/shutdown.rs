//! Worker shutdown release tests: quiesce ordering, unconfirmed cleanup refusal, and cancel precedence.

use super::{fake_runs::FakeRuns, fakes::*};
use crate::{
    host::{Deps, Quiesce, SingleModel, WorkerConfig},
    model::{ModelError, ModelRequest, ModelRunner, RunnerKind, StepFuture},
    worker::{Outcome, Worker},
};
use bot_core::{
    command::*,
    id::*,
    intake::{RequestKey, RequestState, RunProgress},
    port::{Broker, Error, Reply, RunStore},
    state::RunContext,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

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
    fn progress(&self, profile: ProfileId, run: RunId) -> Reply<'_, Option<RunProgress>> {
        self.inner.progress(profile, run)
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
    fn finish_with(
        &self,
        attempt: Attempt,
        done: Finish,
        outcome: RunOutcome,
    ) -> Reply<'_, Completion> {
        self.inner.finish_with(attempt, done, outcome)
    }
    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry> {
        self.inner.fail(attempt, failure)
    }
    fn history(&self, attempt: Attempt, limit: u32) -> Reply<'_, Vec<Turn>> {
        self.inner.history(attempt, limit)
    }
}

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

struct SlowPlan;

impl ModelRunner for SlowPlan {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }
    fn step(&self, _: ModelRequest) -> StepFuture<'_> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            calls("Trying now", &[("r1:0:0", "agenda.list", "{}")])
        })
    }
}

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
        .expect("worker should stop after shutdown")
        .unwrap();
}

fn position(events: &[String], what: &str) -> Option<usize> {
    events.iter().position(|e| e == what)
}

fn nothing_was_reported(s: &Setup) {
    let state = lock(&s.rig.runs.state);
    assert!(state.finished.is_empty(), "must not record completion");
    assert!(state.failed.is_empty(), "must not record failure");
}

#[tokio::test]
async fn a_run_still_going_after_the_grace_is_stopped_confirmed_and_returned_in_that_order() {
    let s = setup(hanging, Some(None), false);
    s.rig.runs.queue(agent_run("r1"));
    serve_then_stop(&s.worker, Duration::from_millis(80)).await;

    let events = s.rig.runs.events();
    let stopped = position(&events, "model_stopped").expect("model stopped");
    let confirmed = position(&events, "quiesce:r1").expect("quiesce confirmed");
    let released = position(&events, "release:r1").expect("run released");
    assert!(
        stopped < confirmed && confirmed < released,
        "must stop -> quiesce -> release in order: {events:?}"
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
        "unconfirmed cleanup must not release: {events:?}"
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
    let s = setup(|_| ScriptedModel::new(vec![say("Done")]), Some(None), false);
    s.rig.runs.queue(agent_run("r1"));
    serve_then_stop(&s.worker, Duration::from_millis(80)).await;

    assert_eq!(
        lock(&s.rig.runs.state).finished.len(),
        1,
        "run should finish normally"
    );
    let events = s.rig.runs.events();
    assert!(position(&events, "quiesce:r1").is_none(), "{events:?}");
    assert!(position(&events, "release:r1").is_none(), "{events:?}");
}

#[tokio::test]
async fn a_tool_call_in_flight_is_dropped_by_the_release_signal_and_the_run_is_returned() {
    let s = setup(
        |_| {
            ScriptedModel::new(vec![calls(
                "Trying now",
                &[("r1:0:0", "agenda.list", "{}")],
            )])
        },
        Some(None),
        true,
    );
    s.rig.runs.queue(agent_run("r1"));
    serve_then_stop(&s.worker, Duration::from_millis(80)).await;

    let events = s.rig.runs.events();
    let started = position(&events, "call_started:agenda.list").expect("tool call started");
    let confirmed = position(&events, "quiesce:r1").expect("quiesce confirmed");
    let released = position(&events, "release:r1").expect("run released");
    assert!(started < confirmed && confirmed < released, "{events:?}");
    nothing_was_reported(&s);
}

#[tokio::test]
async fn a_cancel_request_wins_over_the_release_signal() {
    let s = setup(|_| Arc::new(SlowToStop), Some(None), false);
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
    assert_eq!(outcome, Outcome::Cancelled, "cancel must take priority");
    let events = s.rig.runs.events();
    assert!(position(&events, "release:r1").is_none(), "{events:?}");
}

#[tokio::test]
async fn a_cancel_request_also_wins_at_the_checks_between_the_steps_of_a_run() {
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
    assert_eq!(outcome, Outcome::Cancelled, "cancel must take priority");
    let events = s.rig.runs.events();
    assert!(position(&events, "release:r1").is_none(), "{events:?}");
    assert!(position(&events, "quiesce:r1").is_none(), "{events:?}");
}
