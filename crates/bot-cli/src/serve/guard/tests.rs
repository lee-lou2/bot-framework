use super::*;
use bot_core::{
    command::{JsonPayload, TokenUsage},
    id::{CallId, Generation, ProfileId, ToolId},
    port::Error,
};
use bot_gateway::testing::ports::{MemoryActive, MemoryBroker};
use std::time::Duration;

/// `claim` 호출 수와 마지막 `fail`의 실패, 종료 반납한 시도를 기록하는 실행 저장소
#[derive(Default)]
struct Spy(
    std::sync::atomic::AtomicUsize,
    std::sync::Mutex<Option<Failure>>,
    std::sync::Mutex<Vec<Attempt>>,
    std::sync::Mutex<Vec<RunOutcome>>,
);

fn unavailable<T: Send + 'static>() -> Reply<'static, T> {
    Box::pin(std::future::ready(Err(Error::Unavailable)))
}

impl RunStore for Spy {
    fn accept_with(&self, _: Accept, _: AcceptExtras) -> Reply<'_, RunId> {
        unavailable()
    }
    fn status(&self, _: RequestKey) -> Reply<'_, Option<RequestState>> {
        Box::pin(std::future::ready(Ok(None)))
    }
    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(std::future::ready(Ok(None)))
    }
    fn release_for_shutdown(&self, attempt: Attempt) -> Reply<'_, ()> {
        self.2.lock().unwrap().push(attempt);
        Box::pin(std::future::ready(Ok(())))
    }
    fn renew(&self, _: Attempt) -> Reply<'_, Beat> {
        unavailable()
    }
    fn checkpoint_batch(&self, _: Attempt, _: Vec<Step>) -> Reply<'_, ()> {
        unavailable()
    }
    fn finish(&self, _: Attempt, _: Finish) -> Reply<'_, Completion> {
        unavailable()
    }
    fn finish_with(&self, _: Attempt, _: Finish, outcome: RunOutcome) -> Reply<'_, Completion> {
        self.3.lock().unwrap().push(outcome);
        Box::pin(std::future::ready(Ok(Completion::Succeeded)))
    }
    fn fail(&self, _: Attempt, failure: Failure) -> Reply<'_, Retry> {
        *self.1.lock().unwrap() = Some(failure);
        Box::pin(std::future::ready(Ok(Retry::After(Duration::from_secs(1)))))
    }
    fn history(&self, _: Attempt, _: u32) -> Reply<'_, Vec<Turn>> {
        unavailable()
    }
}

#[tokio::test]
async fn a_stale_worker_claims_nothing_and_leaves_the_work_for_a_current_one() {
    let active = MemoryActive::new(Some("set-1"));
    let spy = Arc::new(Spy::default());
    let runs = GuardedRuns::new(spy.clone(), active.check("set-1"));
    let claims = || spy.0.load(std::sync::atomic::Ordering::SeqCst);
    // 같은 판본: 안쪽 저장소의 claim까지 간다
    runs.claim().await.unwrap();
    assert_eq!(claims(), 1);
    // 다른 프로세스가 집합을 바꿨다: 안쪽까지 가지 않고 «집을 일 없음»이다(일은 저장소에 남는다)
    active.set(Some("set-2"));
    assert!(matches!(runs.claim().await, Ok(None)));
    assert_eq!(claims(), 1, "구판은 저장소에서 일을 집지 않는다");
    // 읽기는 막지 않는다
    let key = RequestKey {
        profile: bot_core::id::ProfileId("personal".into()),
        binding: bot_core::id::BindingId("desk".into()),
        principal: PrincipalId("local:owner".into()),
        request: bot_core::id::RequestId("r1".into()),
    };
    assert!(matches!(runs.status(key).await, Ok(None)));
}

/// 운영 래퍼가 계량을 삼키면 저장소는 잘림·사용량을 영영 받지 못한다(기본 `finish_with`는 계량을 버리고 `finish`로 끝낸다): 같은 완료·같은 계량이 안쪽에 닿아야 한다.
#[tokio::test]
async fn a_finish_with_an_outcome_reaches_the_inner_store_unchanged_even_for_a_stale_worker() {
    let active = MemoryActive::new(Some("set-1"));
    let spy = Arc::new(Spy::default());
    let runs = GuardedRuns::new(spy.clone(), active.check("set-1"));
    let done = || Finish {
        result: JsonPayload(r#"{"text":"답"}"#.into()),
        answers: Vec::new(),
    };
    let outcome = RunOutcome {
        truncated: true,
        usage: Some(TokenUsage {
            prompt: 9,
            completion: 4,
        }),
    };
    runs.finish_with(attempt(), done(), outcome).await.unwrap();
    // 구판이 된 프로세스도 이미 집은 run의 완료는 그대로 저장한다(새 일만 집지 않는다)
    active.set(Some("set-2"));
    runs.finish_with(attempt(), done(), RunOutcome::default())
        .await
        .unwrap();
    assert_eq!(
        *spy.3.lock().unwrap(),
        vec![outcome, RunOutcome::default()],
        "계량이 한 번씩 그대로 닿는다"
    );
    // 계량이 없는 일반 완료는 안쪽의 `finish`로 간다(여기서는 시험용 저장소가 거절한다)
    assert!(matches!(
        runs.finish(attempt(), done()).await,
        Err(Error::Unavailable)
    ));
}

#[tokio::test]
async fn a_stale_worker_still_hands_back_its_own_claim_on_shutdown() {
    let active = MemoryActive::new(Some("set-1"));
    let spy = Arc::new(Spy::default());
    let runs = GuardedRuns::new(spy.clone(), active.check("set-1"));
    // 구판이 됐다: 새 일은 집지 못하지만 이미 집은 claim의 종료 반납은 그대로 안쪽 저장소에 간다
    active.set(Some("set-2"));
    runs.release_for_shutdown(attempt())
        .await
        .expect("기본 구현(Unavailable)으로 새지 않고 넘어간다");
    assert_eq!(*spy.2.lock().unwrap(), [attempt()]);
}

fn attempt() -> Attempt {
    Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        claim: None,
        generation: Generation(1),
    }
}

/// 실패 보고(`fail`)를 정해 둔 순서대로 답하고 호출 횟수를 세는 저장소 쪽 포트
struct FakeHandover {
    answers: std::sync::Mutex<std::collections::VecDeque<Result<Retry, Error>>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl FakeHandover {
    fn answering(answers: impl IntoIterator<Item = Result<Retry, Error>>) -> Arc<Self> {
        Arc::new(Self {
            answers: std::sync::Mutex::new(answers.into_iter().collect()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Handover for FakeHandover {
    fn fail(&self, _: Attempt, _: Failure) -> Reply<'_, Retry> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // 준비한 답이 떨어지면 계속 읽지 못하는 것으로 본다
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(Error::Unavailable));
        Box::pin(std::future::ready(answer))
    }

    fn release(&self, _: Attempt) -> Reply<'_, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

fn guarded_runs(spy: &Arc<Spy>, handover: &Arc<FakeHandover>) -> GuardedRuns {
    GuardedRuns::new(spy.clone(), MemoryActive::new(Some("set-1")).check("set-1"))
        .with_handover(handover.clone())
        .with_retry(3, Duration::from_millis(1))
}

#[tokio::test]
async fn the_store_decides_between_the_hand_back_and_the_ordinary_failure_and_the_inner_path_is_not_used()
 {
    let spy = Arc::new(Spy::default());
    // 반납(구판의 소유자)과 일반 실패의 결과는 저장소가 정한 그대로 돌아온다
    let handover = FakeHandover::answering([
        Ok(Retry::After(Duration::ZERO)),
        Ok(Retry::Dead),
        Ok(Retry::After(Duration::from_secs(5))),
    ]);
    let runs = guarded_runs(&spy, &handover);
    for (failure, expected) in [
        (
            Failure::Permanent("model_denied"),
            Retry::After(Duration::ZERO),
        ),
        (Failure::Transient("x"), Retry::Dead),
        (
            Failure::Transient("x"),
            Retry::After(Duration::from_secs(5)),
        ),
    ] {
        assert_eq!(runs.fail(attempt(), failure).await.unwrap(), expected);
    }
    assert_eq!(handover.calls(), 3);
    assert_eq!(
        spy.1.lock().unwrap().take(),
        None,
        "안쪽 일반 실패 경로를 따로 부르지 않아요"
    );
}

#[tokio::test]
async fn a_decision_that_cannot_be_read_is_repeated_and_never_falls_back_to_an_ordinary_failure() {
    // 활성 집합 행 잠금 시간 초과 같은 일시 오류: 아무것도 기록되지 않았으니 같은 호출을 다시 하고, 성공하면 그 결과다
    let spy = Arc::new(Spy::default());
    let handover = FakeHandover::answering([
        Err(Error::Unavailable),
        Err(Error::Unavailable),
        Ok(Retry::After(Duration::ZERO)),
    ]);
    let runs = guarded_runs(&spy, &handover);
    let retry = runs
        .fail(attempt(), Failure::Permanent("model_denied"))
        .await
        .unwrap();
    assert_eq!(retry, Retry::After(Duration::ZERO));
    assert_eq!(handover.calls(), 3);
    assert_eq!(
        spy.1.lock().unwrap().take(),
        None,
        "읽지 못한 판정 뒤에 일반 실패를 기록하면 안 돼요"
    );
    // 계속 읽지 못하면 오류로 물러난다 — 일반 실패(마지막 시도면 dead)를 기록하지 않는다
    let handover = FakeHandover::answering([]);
    let runs = guarded_runs(&spy, &handover);
    assert!(matches!(
        runs.fail(attempt(), Failure::Permanent("model_denied"))
            .await,
        Err(Error::Unavailable)
    ));
    assert_eq!(handover.calls(), 3, "정한 횟수만큼만 다시 한다");
    assert_eq!(spy.1.lock().unwrap().take(), None);
}

#[tokio::test]
async fn an_owner_refusal_is_returned_at_once_and_a_host_without_the_port_uses_the_inner_failure() {
    // 소유자가 아니다(이미 인계됐다): 다시 해도 같으니 한 번만 묻고 그대로 돌려준다
    let spy = Arc::new(Spy::default());
    let handover = FakeHandover::answering([Err(Error::Stale)]);
    let runs = guarded_runs(&spy, &handover);
    assert!(matches!(
        runs.fail(attempt(), Failure::Transient("x")).await,
        Err(Error::Stale)
    ));
    assert_eq!(handover.calls(), 1);
    // 저장소 포트가 없는 구성(시험·단독)은 보고한 실패를 안쪽에 그대로 맡긴다
    let plain = GuardedRuns::new(spy.clone(), MemoryActive::new(Some("set-1")).check("set-2"));
    plain
        .fail(attempt(), Failure::Permanent("x"))
        .await
        .unwrap();
    assert_eq!(spy.1.lock().unwrap().take(), Some(Failure::Permanent("x")));
}

fn call() -> ToolCall {
    ToolCall {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: None,
            generation: Generation(1),
        },
        call: CallId("c1".into()),
        tool: ToolId("agenda.list".into()),
        args: JsonPayload("{}".into()),
    }
}

#[tokio::test]
async fn a_stale_worker_starts_no_tool_but_still_passes_decisions_through() {
    let active = MemoryActive::new(Some("set-1"));
    let inner = MemoryBroker::new();
    let broker = GuardedBroker::new(inner.clone(), active.check("set-1"));
    // 같은 판본: 도구 호출은 안쪽 브로커까지 간다(가짜 브로커는 일시 실패를 돌려준다)
    assert!(matches!(broker.call(call()).await, Err(Error::Unavailable)));
    // 다른 프로세스가 집합을 바꿨다: 호출은 안쪽까지 가지 않고 일시 실패다
    active.set(Some("set-2"));
    assert!(matches!(broker.call(call()).await, Err(Error::Stale)));
    // 승인 결정은 늘 위임한다
    broker
        .decide(
            ApprovalId("n1".into()),
            PrincipalId("slack:home/U1".into()),
            false,
        )
        .await
        .unwrap();
    assert_eq!(inner.decisions().len(), 1);
}
