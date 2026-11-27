//! End-to-end worker tests over PostgreSQL and BrokerService covering the agenda flow, approval, fencing, delegation, dead-letter resume, and operator cancellation.

use crate::support::*;
use bot_core::{
    command::*,
    id::*,
    port::{AuditLog, Error, RunAdmin, RunStore},
    state::RunState,
    trace::key,
};
use bot_runtime::{
    Models, Outcome, SingleModel,
    model::{
        DemoModel, ModelError, ModelRequest, ModelRunner, ModelStep, PendingCall, RunnerKind,
        StepFuture, Usage,
    },
};
use std::sync::{Arc, Mutex};

fn demo() -> Arc<dyn Models> {
    Arc::new(SingleModel(Arc::new(DemoModel)))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[tokio::test]
async fn the_agenda_flow_lists_events_through_the_real_broker_and_leaves_one_traceable_result() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-1", "Show today's agenda").await;
    let worker = w.worker(&w.pg.store, demo(), config());
    assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
    let state = w.status("req-1").await;
    assert_eq!(state.state, RunState::Succeeded);
    assert!(
        state.result.unwrap().0.contains("Walk"),
        "tool output must appear in reply"
    );
    assert_eq!(w.runner.jobs().len(), 1);

    let spans = w.spans.spans();
    let root = spans
        .iter()
        .find(|s| s.name == "worker.run")
        .expect("root span");
    assert_eq!(root.str(key::BOT_OUTCOME), Some("succeeded"));
    assert_eq!(root.str(key::BOT_RUN_ID), Some(run.0.as_str()));
    let tool = spans
        .iter()
        .find(|s| s.name == "broker.tool.call")
        .expect("tool span");
    assert_eq!(tool.str(key::GEN_AI_OPERATION_NAME), Some("execute_tool"));
    assert_eq!(tool.str(key::GEN_AI_TOOL_TYPE), Some("function"));
    assert_eq!(
        (tool.ids.trace, tool.ids.parent),
        (root.ids.trace, Some(root.ids.span))
    );
    assert_eq!(spans.iter().filter(|s| s.name == "worker.model").count(), 2);
    assert_eq!(w.pg.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_delete_waits_for_approval_then_runs_exactly_once_after_the_same_generation_resumes() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-del", "Delete the walk event").await;
    let worker = w.worker(&w.pg.store, demo(), config());
    assert_eq!(worker.run_once().await, Some(Outcome::Parked));
    assert_eq!(w.pg.state_of(&run).await, "waiting");
    assert_eq!(w.runner.jobs().len(), 1);
    let nonce =
        w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    let generation =
        w.pg.scalar::<i64>(&format!(
            "select generation from bot_runs where id = '{}'",
            run.0
        ))
        .await;
    use bot_core::port::Broker;
    assert!(matches!(
        w.broker
            .decide(
                ApprovalId(nonce.clone()),
                PrincipalId("local:guest".into()),
                true
            )
            .await,
        Err(Error::Denied)
    ));
    w.broker
        .decide(ApprovalId(nonce.clone()), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    assert_eq!(w.pg.state_of(&run).await, "ready");
    assert_eq!(w.runner.jobs().len(), 1);

    assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
    assert_eq!(w.runner.jobs().len(), 2);
    assert_eq!(
        w.pg.scalar::<i64>(&format!(
            "select generation from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        generation,
        "approval resume must keep the same generation"
    );
    let state = w.status("req-del").await;
    assert!(state.result.unwrap().0.contains("Deleted"));
    let keys: Vec<String> = sqlx::query_scalar("select key from bot_steps order by key")
        .fetch_all(w.pg.store.pool())
        .await
        .unwrap();
    assert_eq!(keys, ["end-0", "end-1", "turn-0", "turn-1"]);
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select state from bot_approvals where nonce = '{nonce}'"
        ))
        .await,
        "consumed"
    );
    assert_eq!(w.pg.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn two_workers_racing_on_one_request_execute_it_once() {
    let Some(w) = world().await else {
        return;
    };
    w.accept("req-race", "Show today's agenda").await;
    let peer = w.pg.peer("worker-2").await;
    let (one, two) = (
        w.worker(&w.pg.store, demo(), config()),
        w.worker(&peer, demo(), config()),
    );
    let (a, b) = tokio::join!(one.run_once(), two.run_once());
    let mut outcomes: Vec<_> = [a, b].into_iter().flatten().collect();
    outcomes.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(outcomes, [Outcome::Succeeded]);
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs where state = 'succeeded'")
            .await,
        1
    );
}

struct Says(&'static str);

impl ModelRunner for Says {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }
    fn step(&self, _: ModelRequest) -> StepFuture<'_> {
        let text = self.0.to_owned();
        Box::pin(async move {
            Ok(ModelStep::Final {
                text,
                usage: Usage::default(),
                outcome: Default::default(),
            })
        })
    }
}

#[tokio::test]
async fn fences_expired_workers() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-fence", "Show today's agenda").await;
    let old_store = w.pg.peer("worker-old").await;
    let new_store = w.pg.peer("worker-new").await;
    let zombie_ctx = old_store.claim().await.unwrap().expect("old worker claim");
    w.pg.sql(&format!(
        "update bot_runs set lease_until_ms = bot_now_ms() - 1 where id = '{}'",
        run.0
    ))
    .await;
    let fresh_ctx = new_store.claim().await.unwrap().expect("new worker claim");
    assert_eq!(
        fresh_ctx.attempt.generation.0,
        zombie_ctx.attempt.generation.0 + 1
    );

    let zombie = w.worker(
        &old_store,
        Arc::new(SingleModel(Arc::new(Says("old worker reply")))),
        config(),
    );
    assert_eq!(zombie.execute(zombie_ctx.clone()).await, Outcome::Lost);
    use bot_core::port::Broker;
    let late = w
        .broker
        .call(ToolCall {
            attempt: zombie_ctx.attempt.clone(),
            call: CallId("late".into()),
            tool: ToolId("agenda.list".into()),
            args: JsonPayload("{}".into()),
        })
        .await;
    assert!(matches!(late, Err(Error::Stale)));
    assert_eq!(w.pg.state_of(&run).await, "leased");
    assert!(w.runner.jobs().is_empty());

    let current = w.worker(
        &new_store,
        Arc::new(SingleModel(Arc::new(Says("new worker reply")))),
        config(),
    );
    assert_eq!(current.execute(fresh_ctx).await, Outcome::Succeeded);
    let state = w.status("req-fence").await;
    assert!(state.result.unwrap().0.contains("new worker reply"));
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.succeeded'")
            .await,
        1
    );
}

struct Delegating;

impl ModelRunner for Delegating {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }
    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            let question = request
                .conversation
                .last()
                .map(|m| m.text.clone())
                .unwrap_or_default();
            if question.contains("delegate to research") {
                return Ok(ModelStep::Final {
                    text: "Research result: no events".into(),
                    usage: Usage::default(),
                    outcome: Default::default(),
                });
            }
            match request.exchanges.first() {
                None => Ok(ModelStep::Calls {
                    text: "Delegating to research".into(),
                    calls: vec![PendingCall {
                        call: bot_runtime::model::call_id(&request.attempt.run, 0, 0),
                        tool: ToolId("delegate".into()),
                        args: JsonPayload(
                            serde_json::json!({"agent": "main/research", "instruction": "delegate to research"})
                                .to_string(),
                        ),
                    }],
                    usage: Usage::default(),
                    measured: None,
                    state: None,
                }),
                Some(done) => {
                    let outcome = format!("{:?}", done.calls[0].end);
                    Ok(ModelStep::Final {
                        text: format!("Parent reply: {outcome}"),
                        usage: Usage::default(),
                        outcome: Default::default(),
                    })
                }
            }
        })
    }
}

#[tokio::test]
async fn a_delegating_parent_waits_and_wakes_once_when_the_child_finishes_even_with_two_workers() {
    let Some(w) = world().await else {
        return;
    };
    let parent = w.accept("req-parent", "Ask research about my agenda").await;
    let peer = w.pg.peer("worker-2").await;
    let models: Arc<dyn Models> = Arc::new(SingleModel(Arc::new(Delegating)));
    let (one, two) = (
        w.worker(&w.pg.store, models.clone(), config()),
        w.worker(&peer, models, config()),
    );
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let loops: Vec<_> = [one, two]
        .into_iter()
        .map(|worker| {
            let rx = shutdown.clone();
            tokio::spawn(async move { worker.run(rx).await })
        })
        .collect();
    until("parent and child must both succeed", async || {
        w.pg.scalar::<i64>("select count(*) from bot_runs where state = 'succeeded'")
            .await
            == 2
    })
    .await;
    stop.send(true).unwrap();
    for task in loops {
        task.await.unwrap();
    }
    let state = w.status("req-parent").await;
    assert!(
        state
            .result
            .unwrap()
            .0
            .contains("Research result: no events")
    );
    assert_eq!(
        w.pg.scalar::<i64>(&format!(
            "select attempts::bigint from bot_runs where id = '{}'",
            parent.0
        ))
        .await,
        0
    );
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs where parent is not null")
            .await,
        1
    );
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.wake'")
            .await,
        1
    );
    assert_eq!(w.pg.store.verify().await.unwrap().first_bad, None);
}

struct Flaky(Mutex<bool>);

impl ModelRunner for Flaky {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }
    fn step(&self, _: ModelRequest) -> StepFuture<'_> {
        let fixed = *lock(&self.0);
        Box::pin(async move {
            if fixed {
                Ok(ModelStep::Final {
                    text: "Works now".into(),
                    usage: Usage::default(),
                    outcome: Default::default(),
                })
            } else {
                Err(ModelError::Failed {
                    code: "model_error",
                    transient: false,
                })
            }
        })
    }
}

#[tokio::test]
async fn a_dead_letter_resumes_as_the_same_logical_run_after_the_operator_acts() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-dead", "Show today's agenda").await;
    let model = Arc::new(Flaky(Mutex::new(false)));
    let worker = w.worker(&w.pg.store, Arc::new(SingleModel(model.clone())), config());
    assert!(matches!(worker.run_once().await, Some(Outcome::Failed(_))));
    assert_eq!(w.status("req-dead").await.state, RunState::Dead);
    *lock(&model.0) = true;
    w.pg.store
        .resume(
            ProfileId("personal".into()),
            run.clone(),
            PrincipalId("local:admin".into()),
            "Fixed model config".into(),
        )
        .await
        .unwrap();
    assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
    let state = w.status("req-dead").await;
    assert_eq!(state.run, run);
    assert!(state.result.unwrap().0.contains("Works now"));
}

struct Waits(Arc<std::sync::atomic::AtomicBool>);

impl ModelRunner for Waits {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, mut request: ModelRequest) -> StepFuture<'_> {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            while !*request.cancel.borrow() {
                if request.cancel.changed().await.is_err() {
                    break;
                }
            }
            Err(ModelError::Cancelled)
        })
    }
}

#[tokio::test]
async fn an_operator_cancel_reaches_the_running_model_through_the_heartbeat() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-cancel", "Show today's agenda").await;
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = w.worker(
        &w.pg.store,
        Arc::new(SingleModel(Arc::new(Waits(started.clone())))),
        config(),
    );
    let running = tokio::spawn(async move { worker.run_once().await });
    // Wait until the runner actually enters `step` so cancellation exercises the heartbeat path.
    until("model runner must be executing", async || {
        started.load(std::sync::atomic::Ordering::SeqCst)
    })
    .await;
    w.pg.store
        .cancel(
            ProfileId("personal".into()),
            run.clone(),
            PrincipalId("local:admin".into()),
            "stop".into(),
        )
        .await
        .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), running)
        .await
        .expect("heartbeat must propagate cancellation")
        .unwrap();
    assert_eq!(outcome, Some(Outcome::Cancelled));
    assert_eq!(w.status("req-cancel").await.state, RunState::Cancelled);
}

/// If cancellation lands before the worker's first store read (`fence` returns `Stale`), the owner must finish as `Cancelled` rather than backing off as `Lost`.
#[tokio::test]
async fn an_operator_cancel_that_lands_before_the_workers_first_read_ends_the_run_cancelled() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-early-cancel", "Show today's agenda").await;
    let ctx = w.pg.store.claim().await.unwrap().expect("claimed run");
    assert_eq!(ctx.attempt.run, run);
    w.pg.store
        .cancel(
            ProfileId("personal".into()),
            run.clone(),
            PrincipalId("local:admin".into()),
            "stop".into(),
        )
        .await
        .unwrap();
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = w.worker(
        &w.pg.store,
        Arc::new(SingleModel(Arc::new(Waits(started.clone())))),
        config(),
    );
    assert_eq!(worker.execute(ctx).await, Outcome::Cancelled);
    assert_eq!(
        w.status("req-early-cancel").await.state,
        RunState::Cancelled
    );
    assert!(!started.load(std::sync::atomic::Ordering::SeqCst));
}
