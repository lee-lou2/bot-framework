//! 종료 반납(실제 PG): 같은 활성 집합·시도 한도 1의 워커가 일하는 중에 종료 신호를 받아 유예(`grace`)를 넘기면 모델을 멈추고 정리를 확인한 뒤
//! run이 ready·시도 0으로 돌아가고 다른 워커가 이어서 성공한다. 정리를 확인하지 못하면 반납하지 않는다. 옛 claim의 반납은 새 소유자의 행을 바꾸지 못한다.

mod support;

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
use support::*;
use tokio::sync::{Notify, watch};

fn rev(value: &str) -> Revision {
    Revision(value.into())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 마지막 시도 한도(1)로 활성 집합 `set`에 묶인 저장소 핸들. 인계(리스 만료)가 시도를 쓰면 곧바로 dead letter다.
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

/// 사건을 순서대로 적는 공유 기록
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

/// 중단 신호가 올 때까지 도는 CLI 모양 실행기(정리 전에 멈췄는지 보려고 순서를 남긴다).
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

/// 정리 확인 입구: 부른 시도를 적고 `result`를 낸다. `gate`가 있으면 시험이 허락할 때까지 기다린다(그 사이 소유권을 바꾸려고).
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
    .unwrap_or_else(|_| panic!("기다리던 일이 일어나지 않았다: {what}"));
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
    let run = w.accept("req-1", "오래 걸리는 일").await;
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
    wait_for("모델이 시작됨", || {
        log.position("model_started").is_some()
    })
    .await;
    let held = generation(&w, &run).await;
    w.pg.assert_state(&run, "leased").await;

    // 종료 신호: 유예(300ms) 안에 끝나지 않으니 정리하고 돌려준다
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("워커가 끝난다")
        .unwrap();

    w.pg.assert_state(&run, "ready").await;
    assert_eq!(attempts(&w, &run).await, 0, "시도를 쓰지 않았다");
    assert_eq!(generation(&w, &run).await, held + 1, "세대를 한 번 올렸다");
    let order = (log.position("model_stopped"), log.position("quiesce"));
    assert!(
        matches!(order, (Some(stopped), Some(confirmed)) if stopped < confirmed),
        "모델을 멈춘 뒤에 정리를 확인했다: {:?}",
        log.all()
    );
    assert_eq!(lock(&quiet.attempts).len(), 1);
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.release'")
            .await,
        1
    );

    // 다른 워커가 이어받아 끝낸다: 세대는 또 오르지 않고 시도는 여전히 0이다
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
    let run = w.accept("req-1", "오래 걸리는 일").await;
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
    wait_for("모델이 시작됨", || {
        log.position("model_started").is_some()
    })
    .await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("워커가 끝난다")
        .unwrap();

    // 확인하지 못했으니 반납하지 않는다: run은 leased로 남고 리스 만료가 인계한다
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
        "같은 집합의 만료라 시도를 쓰고 한도 1이라 dead letter다(인계 규칙 그대로)"
    );
    w.pg.assert_state(&run, "dead").await;
}

#[tokio::test]
async fn a_claim_taken_over_during_the_cleanup_is_not_touched_by_the_late_release() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-1", "오래 걸리는 일").await;
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
    wait_for("모델이 시작됨", || {
        log.position("model_started").is_some()
    })
    .await;
    stop.send(true).unwrap();
    // 정리 확인이 진행 중인 동안(허락 전) 리스가 끝나 다른 워커가 인계한다(한도를 넉넉히 둔 핸들이라 시도를 써도 살아 있다)
    tokio::time::timeout(Duration::from_secs(10), quiet.called.notified())
        .await
        .expect("정리 확인이 불리지 않았다");
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
        .expect("다른 워커가 인계한다");
    assert_eq!(taken.attempt.run, run);

    // 이제 늦은 정리가 끝나 옛 시도가 반납을 시도한다: 소유자가 아니니 아무것도 바꾸지 못한다
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("워커가 끝난다")
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
        taken.attempt.claim.unwrap().0,
        "새 소유자의 행을 건드리지 않았다"
    );
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.release'")
            .await,
        0
    );
    // 새 소유자는 그대로 일을 끝낼 수 있다
    RunStore::finish(&*other, taken.attempt.clone(), done_text("이어서 마쳤어요"))
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

/// 첫 차례에 읽기 도구 하나를 부르고, 결과를 받은 다음 차례에 마무리 글을 내는 Step 실행기.
struct OneCall;

impl ModelRunner for OneCall {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }
    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            if request.exchanges.is_empty() {
                Ok(ModelStep::Calls {
                    text: "목록을 볼게요".into(),
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
                    text: "마쳤어요".into(),
                    usage: Usage::default(),
                    outcome: Default::default(),
                })
            }
        })
    }
}

/// 운영 호스트의 도구 쪽 정리와 같은 일을 가짜 실행기로 한다: 이 시도가 시작해 둔 호출의 소유 표지를 저장소에서 읽어 `reap`한다.
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

/// 읽기 도구를 시작했지만 끝나지 않은 채 종료 신호를 받은 worker가 정리 뒤 run을 돌려준 상태까지 만든다. 돌려주는 값은 (run, 시작한 세대).
async fn stopped_with_a_started_tool(w: &World) -> (RunId, i64) {
    let run = w.accept("req-1", "일정 알려줘").await;
    let _gate = w.runner.hold_next(); // 첫 도구 실행은 끝나지 않는다
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
    wait_for("도구가 시작됨", || w.runner.jobs().len() == 1).await;
    let held = generation(w, &run).await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("워커가 끝난다")
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
        [format!("{}.{}.{}", run.0, held, call_id(&run, 0, 0).0)],
        "반납 전에 시작해 둔 호출의 프로세스를 끝내고 확인했다"
    );
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "started",
        "호출은 시작한 채로 남는다 — 다음 점유의 복구가 판정한다"
    );

    // 다음 점유: 옛 프로세스 종료를 증명할 수 있으니 같은 호출을 한 번 더 실행해 끝낸다(읽기 도구)
    let next = handle(&w, "new-worker", "set-1");
    let resumed = w.worker(&next, Arc::new(SingleModel(Arc::new(OneCall))), cfg());
    assert_eq!(resumed.run_once().await, Some(Outcome::Succeeded));
    assert_eq!(w.runner.jobs().len(), 2, "증명한 뒤에 한 번 더 실행했다");
    w.pg.assert_state(&run, "succeeded").await;
}

#[tokio::test]
async fn the_next_claim_does_not_rerun_a_started_tool_when_it_cannot_prove_the_old_process_ended() {
    let Some(w) = world().await else {
        return;
    };
    let (run, _) = stopped_with_a_started_tool(&w).await;
    w.pg.assert_state(&run, "ready").await;

    // 다음 호스트는 옛 프로세스의 종료를 확인하지 못한다: 결과를 모르는 호출을 다시 실행하지 않고 보류한다
    w.runner.set_reap_ok(false);
    let next = handle(&w, "new-worker", "set-1");
    let resumed = w.worker(&next, Arc::new(SingleModel(Arc::new(OneCall))), cfg());
    assert_eq!(resumed.run_once().await, Some(Outcome::Parked));
    assert_eq!(w.runner.jobs().len(), 1, "다시 실행하지 않았다");
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "held"
    );
}
