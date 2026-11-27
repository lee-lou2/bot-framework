//! PostgreSQL tests for graceful shutdown release: after `grace` expires, the worker stops the model, confirms quiesce, and releases the run to `ready` with zero attempts spent.
//! Unconfirmed quiesce withholds release so lease expiry handles takeover; stale claims cannot release runs already taken over.

use crate::support::*;
use bot_broker::ports::ToolRunner;
use bot_core::{
    command::JsonPayload,
    id::*,
    port::{Error, Reply, RunStore},
};
use bot_runtime::{
    Models, Outcome, Quiesce, SingleModel, WorkerConfig,
    model::{
        DemoModel, ModelError, ModelRequest, ModelRunner, ModelStep, PendingCall, RunnerKind,
        StepFuture, Usage, call_id,
    },
};
use bot_store::{Config, Store};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, watch};

fn rev(value: &str) -> Revision {
    Revision(value.into())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Store handle bound to activation `set` with `max_attempts = 1` so any attempt-consuming expiry dead-letters the run.
fn handle(w: &World, worker: &str, set: &str) -> Arc<Store> {
    Arc::new(
        Store::from_pool(
            w.pg.store.pool().clone(),
            Config {
                worker: worker.into(),
                max_attempts: 1,
                ..Config::default()
            },
        )
        .for_activation(rev(set)),
    )
}

fn cfg() -> WorkerConfig {
    WorkerConfig {
        grace: Duration::from_millis(300),
        cleanup: Duration::from_secs(5),
        ..config()
    }
}

#[derive(Default, Clone)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn push(&self, what: impl Into<String>) {
        lock(&self.0).push(what.into());
    }
    fn all(&self) -> Vec<String> {
        lock(&self.0).clone()
    }
    fn position(&self, what: &str) -> Option<usize> {
        self.all().iter().position(|e| e == what)
    }
}

struct Hanging(Log);

impl ModelRunner for Hanging {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, mut request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            self.0.push("model_started");
            while !*request.cancel.borrow() {
                if request.cancel.changed().await.is_err() {
                    break;
                }
            }
            self.0.push("model_stopped");
            Err(ModelError::Cancelled)
        })
    }
}

struct Quiet {
    log: Log,
    attempts: Mutex<Vec<Attempt>>,
    result: Mutex<Option<Error>>,
    called: Notify,
    gate: Option<Arc<Notify>>,
}

impl Quiet {
    fn new(log: &Log, result: Option<Error>, gate: Option<Arc<Notify>>) -> Arc<Quiet> {
        Arc::new(Quiet {
            log: log.clone(),
            attempts: Mutex::new(Vec::new()),
            result: Mutex::new(result),
            called: Notify::new(),
            gate,
        })
    }
}

impl Quiesce for Quiet {
    fn quiesce(&self, attempt: Attempt) -> Reply<'_, ()> {
        self.log.push("quiesce");
        lock(&self.attempts).push(attempt);
        self.called.notify_one();
        Box::pin(async move {
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            lock(&self.result).take().map_or(Ok(()), Err)
        })
    }
}

fn hanging(log: &Log) -> Arc<dyn Models> {
    Arc::new(SingleModel(Arc::new(Hanging(log.clone()))))
}

fn demo() -> Arc<dyn Models> {
    Arc::new(SingleModel(Arc::new(DemoModel)))
}

async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
}

async fn attempts(w: &World, run: &RunId) -> i32 {
    w.pg.scalar::<i32>(&format!(
        "select attempts from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

async fn generation(w: &World, run: &RunId) -> i64 {
    w.pg.scalar::<i64>(&format!(
        "select generation from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

#[tokio::test]
async fn a_worker_that_gets_the_stop_signal_mid_run_returns_it_after_the_grace_and_another_worker_finishes_it()
 {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-1", "Long-running task").await;
    let log = Log::default();
    let quiet = Quiet::new(&log, None, None);
    let old = handle(&w, "old-worker", "set-1");
    let worker = w
        .worker(&old, hanging(&log), cfg())
        .with_quiesce(quiet.clone());
    let (stop, shutdown) = watch::channel(false);
    let serving = {
        let worker = worker.clone();
        tokio::spawn(async move { worker.run(shutdown).await })
    };
    wait_for("model started", || log.position("model_started").is_some()).await;
    let held = generation(&w, &run).await;
    w.pg.assert_state(&run, "leased").await;

    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("worker must exit")
        .unwrap();

    w.pg.assert_state(&run, "ready").await;
    assert_eq!(attempts(&w, &run).await, 0);
    assert_eq!(generation(&w, &run).await, held + 1);
    let order = (log.position("model_stopped"), log.position("quiesce"));
    assert!(
        matches!(order, (Some(stopped), Some(confirmed)) if stopped < confirmed),
        "quiesce must run after model stops: {:?}",
        log.all()
    );
    assert_eq!(lock(&quiet.attempts).len(), 1);
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.release'")
            .await,
        1
    );

    let next = handle(&w, "new-worker", "set-1");
    let resumed = w.worker(&next, demo(), cfg());
    assert_eq!(resumed.run_once().await, Some(Outcome::Succeeded));
    w.pg.assert_state(&run, "succeeded").await;
    assert_eq!(generation(&w, &run).await, held + 1);
    assert_eq!(attempts(&w, &run).await, 0);
}

#[tokio::test]
async fn an_unconfirmed_cleanup_leaves_the_lease_to_expire_and_the_handover_spends_the_attempt() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-1", "Long-running task").await;
    let log = Log::default();
    let old = handle(&w, "old-worker", "set-1");
    let worker = w
        .worker(&old, hanging(&log), cfg())
        .with_quiesce(Quiet::new(&log, Some(Error::Unavailable), None));
    let (stop, shutdown) = watch::channel(false);
    let serving = {
        let worker = worker.clone();
        tokio::spawn(async move { worker.run(shutdown).await })
    };
    wait_for("model started", || log.position("model_started").is_some()).await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("worker must exit")
        .unwrap();

    w.pg.assert_state(&run, "leased").await;
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.release'")
            .await,
        0
    );
    w.pg.sql(&format!(
        "update bot_runs set lease_until_ms = bot_now_ms() - 1 where id = '{}'",
        run.0
    ))
    .await;
    let next = handle(&w, "new-worker", "set-1");
    assert!(
        next.claim().await.unwrap().is_none(),
        "same-activation expiry consumes the sole attempt and dead-letters the run"
    );
    w.pg.assert_state(&run, "dead").await;
}

#[tokio::test]
async fn a_claim_taken_over_during_the_cleanup_is_not_touched_by_the_late_release() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-1", "Long-running task").await;
    let log = Log::default();
    let gate = Arc::new(Notify::new());
    let quiet = Quiet::new(&log, None, Some(gate.clone()));
    let old = handle(&w, "old-worker", "set-1");
    let worker = w
        .worker(&old, hanging(&log), cfg())
        .with_quiesce(quiet.clone());
    let (stop, shutdown) = watch::channel(false);
    let serving = {
        let worker = worker.clone();
        tokio::spawn(async move { worker.run(shutdown).await })
    };
    wait_for("model started", || log.position("model_started").is_some()).await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), quiet.called.notified())
        .await
        .expect("quiesce must be called");
    w.pg.sql(&format!(
        "update bot_runs set lease_until_ms = bot_now_ms() - 1 where id = '{}'",
        run.0
    ))
    .await;
    let other = Arc::new(
        Store::from_pool(
            w.pg.store.pool().clone(),
            Config {
                worker: "other-worker".into(),
                ..Config::default()
            },
        )
        .for_activation(rev("set-1")),
    );
    let taken = RunStore::claim(&*other)
        .await
        .unwrap()
        .expect("other worker takeover");
    assert_eq!(taken.attempt.run, run);

    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("worker must exit")
        .unwrap();
    w.pg.assert_state(&run, "leased").await;
    assert_eq!(
        generation(&w, &run).await as u64,
        taken.attempt.generation.0
    );
    assert_eq!(
        w.pg.scalar::<i64>(&format!(
            "select claim_epoch from bot_runs where id = '{}'",
            run.0
        ))
        .await as u64,
        taken.attempt.claim.unwrap().0
    );
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.release'")
            .await,
        0
    );
    RunStore::finish(
        &*other,
        taken.attempt.clone(),
        done_text("finished after takeover"),
    )
    .await
    .unwrap();
    w.pg.assert_state(&run, "succeeded").await;
}

fn done_text(text: &str) -> bot_core::command::Finish {
    let payload = bot_core::command::JsonPayload(format!(r#"{{"text":"{text}"}}"#));
    bot_core::command::Finish {
        result: payload.clone(),
        answers: vec![payload],
    }
}

struct OneCall;

impl ModelRunner for OneCall {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }
    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            if request.exchanges.is_empty() {
                Ok(ModelStep::Calls {
                    text: "Listing events".into(),
                    calls: vec![PendingCall {
                        call: call_id(&request.attempt.run, 0, 0),
                        tool: ToolId("agenda.list".into()),
                        args: JsonPayload("{}".into()),
                    }],
                    usage: Usage::default(),
                    measured: None,
                    state: None,
                })
            } else {
                Ok(ModelStep::Final {
                    text: "Done".into(),
                    usage: Usage::default(),
                    outcome: Default::default(),
                })
            }
        })
    }
}

struct ReapTools {
    store: Arc<Store>,
    runner: Arc<bot_broker::testing::FakeRunner>,
}

impl Quiesce for ReapTools {
    fn quiesce(&self, attempt: Attempt) -> Reply<'_, ()> {
        Box::pin(async move {
            for owner in self.store.started_owners(&attempt).await? {
                self.runner.reap(owner).await?;
            }
            Ok(())
        })
    }
}

async fn stopped_with_a_started_tool(w: &World) -> (RunId, i64) {
    let run = w.accept("req-1", "Show today's agenda").await;
    let _gate = w.runner.hold_next();
    let old = handle(w, "old-worker", "set-1");
    let worker = w
        .worker(&old, Arc::new(SingleModel(Arc::new(OneCall))), cfg())
        .with_quiesce(Arc::new(ReapTools {
            store: old.clone(),
            runner: w.runner.clone(),
        }));
    let (stop, shutdown) = watch::channel(false);
    let serving = {
        let worker = worker.clone();
        tokio::spawn(async move { worker.run(shutdown).await })
    };
    wait_for("tool started", || w.runner.jobs().len() == 1).await;
    let held = generation(w, &run).await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("worker must exit")
        .unwrap();
    (run, held)
}

#[tokio::test]
async fn a_tool_in_flight_at_the_stop_signal_is_reaped_before_the_release_and_rerun_only_after_the_proof()
 {
    let Some(w) = world().await else {
        return;
    };
    let (run, held) = stopped_with_a_started_tool(&w).await;
    w.pg.assert_state(&run, "ready").await;
    assert_eq!(attempts(&w, &run).await, 0);
    assert_eq!(generation(&w, &run).await, held + 1);
    assert_eq!(
        w.runner.reaped(),
        [format!("{}.{}.{}", run.0, held, call_id(&run, 0, 0).0)]
    );
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "started"
    );

    let next = handle(&w, "new-worker", "set-1");
    let resumed = w.worker(&next, Arc::new(SingleModel(Arc::new(OneCall))), cfg());
    assert_eq!(resumed.run_once().await, Some(Outcome::Succeeded));
    assert_eq!(w.runner.jobs().len(), 2);
    w.pg.assert_state(&run, "succeeded").await;
}

#[tokio::test]
async fn the_next_claim_does_not_rerun_a_started_tool_when_it_cannot_prove_the_old_process_ended() {
    let Some(w) = world().await else {
        return;
    };
    let (run, _) = stopped_with_a_started_tool(&w).await;
    w.pg.assert_state(&run, "ready").await;

    w.runner.set_reap_ok(false);
    let next = handle(&w, "new-worker", "set-1");
    let resumed = w.worker(&next, Arc::new(SingleModel(Arc::new(OneCall))), cfg());
    assert_eq!(resumed.run_once().await, Some(Outcome::Parked));
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "held"
    );
}
