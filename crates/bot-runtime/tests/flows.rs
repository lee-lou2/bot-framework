//! 워커 종단 시험: 실제 PG 저장소와 실제 BrokerService(가짜 실행기) 위에서 README 흐름·인계·위임·취소를 확인한다.

mod support;

use bot_core::{
    command::*,
    id::*,
    port::{Error, RunAdmin, RunStore},
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
use support::*;

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
    let run = w.accept("req-1", "오늘 일정 알려줘").await;
    let worker = w.worker(&w.pg.store, demo(), config());
    assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
    let state = w.status("req-1").await;
    assert_eq!(state.state, RunState::Succeeded);
    assert!(
        state.result.unwrap().0.contains("산책"),
        "도구 결과가 답에 들어갔다"
    );
    assert_eq!(w.runner.jobs().len(), 1, "도구는 한 번");

    // 한 시도의 스팬이 한 추적에 모인다: 워커 루트 아래에 모델 차례와 브로커의 도구 호출이 붙는다
    let spans = w.spans.spans();
    let root = spans
        .iter()
        .find(|s| s.name == "worker.run")
        .expect("루트 스팬");
    assert_eq!(root.str(key::BOT_OUTCOME), Some("succeeded"));
    assert_eq!(root.str(key::BOT_RUN_ID), Some(run.0.as_str()));
    let tool = spans
        .iter()
        .find(|s| s.name == "broker.tool.call")
        .expect("도구 스팬");
    assert_eq!(tool.str(key::GEN_AI_OPERATION_NAME), Some("execute_tool"));
    assert_eq!(tool.str(key::GEN_AI_TOOL_TYPE), Some("function"));
    assert_eq!(
        (tool.ids.trace, tool.ids.parent),
        (root.ids.trace, Some(root.ids.span))
    );
    assert_eq!(spans.iter().filter(|s| s.name == "worker.model").count(), 2);
    assert_eq!(w.pg.store.verify().await.unwrap().first_bad, None);
}

use bot_core::port::AuditLog;

#[tokio::test]
async fn a_delete_waits_for_approval_then_runs_exactly_once_after_the_same_generation_resumes() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-del", "산책 일정을 삭제해줘").await;
    let worker = w.worker(&w.pg.store, demo(), config());
    // 목록 조회까지 하고 삭제는 승인을 기다린다(워커는 리스를 놓고 물러난다)
    assert_eq!(worker.run_once().await, Some(Outcome::Parked));
    assert_eq!(w.pg.state_of(&run).await, "waiting");
    assert_eq!(w.runner.jobs().len(), 1, "승인 전에는 목록 조회뿐이다");
    let nonce =
        w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    let generation =
        w.pg.scalar::<i64>(&format!(
            "select generation from bot_runs where id = '{}'",
            run.0
        ))
        .await;
    // 낯선 사람의 클릭은 거절, 소유자의 클릭은 run을 깨운다(결정만으로는 실행하지 않는다)
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

    // 같은 세대로 재개한 워커가 체크포인트를 되살려 목록은 다시 부르지 않고 삭제만 한 번 실행한다
    assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
    assert_eq!(w.runner.jobs().len(), 2, "목록 1 + 삭제 1");
    assert_eq!(
        w.pg.scalar::<i64>(&format!(
            "select generation from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        generation,
        "승인 재개는 세대를 올리지 않는다"
    );
    let state = w.status("req-del").await;
    assert!(state.result.unwrap().0.contains("삭제했어요"));
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
    w.accept("req-race", "오늘 일정 알려줘").await;
    let peer = w.pg.peer("worker-2").await;
    let (one, two) = (
        w.worker(&w.pg.store, demo(), config()),
        w.worker(&peer, demo(), config()),
    );
    let (a, b) = tokio::join!(one.run_once(), two.run_once());
    let mut outcomes: Vec<_> = [a, b].into_iter().flatten().collect();
    outcomes.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(
        outcomes,
        [Outcome::Succeeded],
        "집은 쪽은 하나, 다른 쪽은 집을 것이 없다"
    );
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs where state = 'succeeded'")
            .await,
        1
    );
}

/// 지정한 글을 그대로 답하는 Step 실행기.
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
    let run = w.accept("req-fence", "오늘 일정 알려줘").await;
    let old_store = w.pg.peer("worker-old").await;
    let new_store = w.pg.peer("worker-new").await;
    let zombie_ctx = old_store.claim().await.unwrap().expect("옛 워커가 집었다");
    // 옛 워커가 멈췄다(리스 만료): 새 워커가 인계하며 세대가 오른다
    w.pg.sql(&format!(
        "update bot_runs set lease_until_ms = bot_now_ms() - 1 where id = '{}'",
        run.0
    ))
    .await;
    let fresh_ctx = new_store
        .claim()
        .await
        .unwrap()
        .expect("새 워커가 인계했다");
    assert_eq!(
        fresh_ctx.attempt.generation.0,
        zombie_ctx.attempt.generation.0 + 1
    );

    // 깨어난 옛 워커의 모든 쓰기는 거절되고 아무것도 남기지 않는다
    let zombie = w.worker(
        &old_store,
        Arc::new(SingleModel(Arc::new(Says("옛 워커의 답")))),
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

    // 새 소유자는 끝까지 간다: 결과는 하나이고 옛 워커의 답이 아니다
    let current = w.worker(
        &new_store,
        Arc::new(SingleModel(Arc::new(Says("새 워커의 답")))),
        config(),
    );
    assert_eq!(current.execute(fresh_ctx).await, Outcome::Succeeded);
    let state = w.status("req-fence").await;
    assert!(state.result.unwrap().0.contains("새 워커의 답"));
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.succeeded'")
            .await,
        1
    );
}

/// 부모는 `main/research`에게 일정 조회를 맡기고 그 결과로 답한다. 자식은 곧바로 답한다.
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
            if question.contains("연구에게 맡기기") {
                return Ok(ModelStep::Final {
                    text: "연구 결과: 일정 없음".into(),
                    usage: Usage::default(),
                    outcome: Default::default(),
                });
            }
            match request.exchanges.first() {
                None => Ok(ModelStep::Calls {
                    text: "연구에게 맡길게요".into(),
                    calls: vec![PendingCall {
                        call: bot_runtime::model::call_id(&request.attempt.run, 0, 0),
                        tool: ToolId("delegate".into()),
                        args: JsonPayload(
                            serde_json::json!({"agent": "main/research", "instruction": "연구에게 맡기기"})
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
                        text: format!("부모의 답: {outcome}"),
                        usage: Usage::default(), outcome: Default::default(),
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
    let parent = w.accept("req-parent", "일정을 연구에게 알아봐 줘").await;
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
    until("부모와 자식이 모두 끝나야 한다", async || {
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
        state.result.unwrap().0.contains("연구 결과: 일정 없음"),
        "자식의 결과가 부모의 답에 들어갔다"
    );
    assert_eq!(
        w.pg.scalar::<i64>(&format!(
            "select attempts::bigint from bot_runs where id = '{}'",
            parent.0
        ))
        .await,
        0,
        "대기·재개는 시도를 쓰지 않는다"
    );
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs where parent is not null")
            .await,
        1,
        "자식은 하나"
    );
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.wake'")
            .await,
        1,
        "부모는 한 번만 깨어났다"
    );
    assert_eq!(w.pg.store.verify().await.unwrap().first_bad, None);
}

/// 처음에는 영구 오류로 끝나고, 고친 뒤에는 답한다.
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
                    text: "이제 돼요".into(),
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
    let run = w.accept("req-dead", "오늘 일정 알려줘").await;
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
            "모델 설정을 고쳤어요".into(),
        )
        .await
        .unwrap();
    assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
    let state = w.status("req-dead").await;
    assert_eq!(state.run, run, "같은 논리 run");
    assert!(state.result.unwrap().0.contains("이제 돼요"));
}

/// 취소 신호가 올 때까지 기다리는 CLI 모양 실행기. 불리면 `started`를 올려 시험이 «모델이 실제로 돌고 있다»를 시간 추정 없이 안다.
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
    let run = w.accept("req-cancel", "오늘 일정 알려줘").await;
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = w.worker(
        &w.pg.store,
        Arc::new(SingleModel(Arc::new(Waits(started.clone())))),
        config(),
    );
    let running = tokio::spawn(async move { worker.run_once().await });
    // `leased`는 집었다는 뜻일 뿐 워커가 아직 첫 저장소 읽기 전일 수 있다: 모델이 실제로 불린 뒤에 취소해야 «실행 중인 모델에 하트비트가 닿는다»를 본다
    until("모델이 실제로 돌고 있어야 한다", async || {
        started.load(std::sync::atomic::Ordering::SeqCst)
    })
    .await;
    w.pg.store
        .cancel(
            ProfileId("personal".into()),
            run.clone(),
            PrincipalId("local:admin".into()),
            "그만".into(),
        )
        .await
        .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), running)
        .await
        .expect("하트비트가 취소를 알려 실행기가 멈춰야 한다")
        .unwrap();
    assert_eq!(outcome, Some(Outcome::Cancelled));
    assert_eq!(w.status("req-cancel").await.state, RunState::Cancelled);
}

/// 운영자의 취소가 워커의 첫 저장소 읽기보다 먼저 닿으면: 저장소는 취소 요청이 선 run의 새 읽기를 `Stale`로 거절한다(`fence`). 리스를 잃은 것(`Lost`)이 아니라
/// 아직 소유자이므로 바로 취소로 끝나야 한다(그대로 물러나면 리스 30초가 지나 다른 워커가 집을 때까지 run이 `leased`로 남는다).
#[tokio::test]
async fn an_operator_cancel_that_lands_before_the_workers_first_read_ends_the_run_cancelled() {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-early-cancel", "오늘 일정 알려줘").await;
    let ctx = w.pg.store.claim().await.unwrap().expect("집은 run");
    assert_eq!(ctx.attempt.run, run);
    w.pg.store
        .cancel(
            ProfileId("personal".into()),
            run.clone(),
            PrincipalId("local:admin".into()),
            "그만".into(),
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
    assert!(
        !started.load(std::sync::atomic::Ordering::SeqCst),
        "모델은 부르지 않았다"
    );
}
