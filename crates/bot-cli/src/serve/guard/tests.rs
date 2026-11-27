use super::*;
use bot_core::{
    command::{JsonPayload, TokenUsage},
    id::{CallId, Generation, ProfileId, ToolId},
    port::Error,
};
use bot_gateway::testing::ports::{MemoryActive, MemoryBroker};
use std::time::Duration;

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
    fn progress(&self, _: ProfileId, _: RunId) -> Reply<'_, Option<RunProgress>> {
        Box::pin(std::future::ready(Ok(Some(RunProgress {
            state: bot_core::state::RunState::Leased,
            cancel_requested: false,
            active_agent: None,
            active_tool: Some("agenda.list".into()),
            waiting_approval: false,
        }))))
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
    runs.claim().await.unwrap();
    assert_eq!(claims(), 1);
    active.set(Some("set-2"));
    assert!(matches!(runs.claim().await, Ok(None)));
    assert_eq!(claims(), 1, "stale worker must not claim work");
    let key = RequestKey {
        profile: bot_core::id::ProfileId("personal".into()),
        binding: bot_core::id::BindingId("desk".into()),
        principal: PrincipalId("local:owner".into()),
        request: bot_core::id::RequestId("r1".into()),
    };
    assert!(matches!(runs.status(key).await, Ok(None)));
    let progress = runs
        .progress(ProfileId("personal".into()), RunId("r1".into()))
        .await
        .unwrap()
        .expect("inner store progress");
    assert_eq!(progress.active_tool.as_deref(), Some("agenda.list"));
}

#[tokio::test]
async fn a_finish_with_an_outcome_reaches_the_inner_store_unchanged_even_for_a_stale_worker() {
    let active = MemoryActive::new(Some("set-1"));
    let spy = Arc::new(Spy::default());
    let runs = GuardedRuns::new(spy.clone(), active.check("set-1"));
    let done = || Finish {
        result: JsonPayload(r#"{"text":"ok"}"#.into()),
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
    // A stale worker still persists completion for already-claimed runs.
    active.set(Some("set-2"));
    runs.finish_with(attempt(), done(), RunOutcome::default())
        .await
        .unwrap();
    assert_eq!(
        *spy.3.lock().unwrap(),
        vec![outcome, RunOutcome::default()],
        "usage and truncation outcome must pass through unchanged"
    );
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
    active.set(Some("set-2"));
    runs.release_for_shutdown(attempt())
        .await
        .expect("must delegate instead of falling through to default Unavailable");
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
        "must not call inner ordinary fail path"
    );
}

#[tokio::test]
async fn a_decision_that_cannot_be_read_is_repeated_and_never_falls_back_to_an_ordinary_failure() {
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
        "must not fall back to ordinary fail on transient handover read error"
    );
    let handover = FakeHandover::answering([]);
    let runs = guarded_runs(&spy, &handover);
    assert!(matches!(
        runs.fail(attempt(), Failure::Permanent("model_denied"))
            .await,
        Err(Error::Unavailable)
    ));
    assert_eq!(handover.calls(), 3, "retried exact configured count");
    assert_eq!(spy.1.lock().unwrap().take(), None);
}

#[tokio::test]
async fn an_owner_refusal_is_returned_at_once_and_a_host_without_the_port_uses_the_inner_failure() {
    let spy = Arc::new(Spy::default());
    let handover = FakeHandover::answering([Err(Error::Stale)]);
    let runs = guarded_runs(&spy, &handover);
    assert!(matches!(
        runs.fail(attempt(), Failure::Transient("x")).await,
        Err(Error::Stale)
    ));
    assert_eq!(handover.calls(), 1);
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
    assert!(matches!(broker.call(call()).await, Err(Error::Unavailable)));
    active.set(Some("set-2"));
    assert!(matches!(broker.call(call()).await, Err(Error::Stale)));
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

#[tokio::test]
async fn only_a_current_worker_can_forward_a_model_call_to_the_broker() {
    use bot_core::cli::CliRefusal;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct ModelBroker(AtomicUsize);
    impl Broker for ModelBroker {
        fn call(&self, _: ToolCall) -> Reply<'_, CallResult> {
            panic!("must use model-specific call_for_model port");
        }
        fn call_for_model(&self, _: ToolCall) -> Reply<'_, ModelCall> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(ModelCall::Refused(CliRefusal::Denied)) })
        }
        fn decide(&self, _: ApprovalId, _: PrincipalId, _: bool) -> Reply<'_, ()> {
            unavailable()
        }
    }

    let active = MemoryActive::new(Some("set-1"));
    let inner = Arc::new(ModelBroker::default());
    let outer = GuardedBroker::new(inner.clone(), active.check("set-1"));
    assert!(matches!(
        outer.call_for_model(call()).await,
        Ok(ModelCall::Refused(CliRefusal::Denied))
    ));
    active.set(Some("set-2"));
    assert!(matches!(
        outer.call_for_model(call()).await,
        Err(Error::Stale)
    ));
    assert_eq!(inner.0.load(Ordering::SeqCst), 1);
}
