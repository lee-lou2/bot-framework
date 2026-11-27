//! `SlackGateway::with_ux` 통합 시험 공용 대역(`UxStore`·`FakeAdmin`·`UnavailableActive`·`UxRig`).
use super::{default_secrets, host, personal};
use bot_core::{
    command::{
        Accept, AcceptExtras, Beat, Completion, Failure, Finish, Retry, RunOutcome, Step, Turn,
    },
    id::{Attempt, PrincipalId, ProfileId, RunId},
    intake::{RequestKey, RequestState, RunProgress},
    port::{Error, Reply, RunAdmin, RunStore},
    state::{RunContext, RunState},
};
use bot_gateway::{
    active::ActiveCheck,
    ingress::Ingress,
    slack::{SlackError, SlackGateway, api::SlackApi},
    testing::{
        ports::{MemoryBroker, MemoryLedger, MemoryThreads},
        slack::FakeSlack,
        *,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Semaphore, task::JoinHandle};
use tokio_util::sync::CancellationToken;

/// `MemoryStore`에 `progress` 제어를 얹은 시험용 저장소.
#[derive(Default)]
pub struct UxStore {
    pub inner: Arc<MemoryStore>,
    progress: Mutex<HashMap<RunId, RunProgress>>,
}

impl UxStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(),
            progress: Mutex::new(HashMap::new()),
        })
    }

    pub fn set_progress(&self, run: &RunId, state: RunState) {
        self.progress.lock().unwrap().insert(
            run.clone(),
            RunProgress {
                state,
                cancel_requested: false,
                active_agent: None,
                active_tool: None,
                waiting_approval: false,
            },
        );
    }
}

impl RunStore for UxStore {
    fn accept_with(&self, command: Accept, extras: AcceptExtras) -> Reply<'_, RunId> {
        let fut = self.inner.accept_with(command, extras);
        Box::pin(async move {
            let run = fut.await?;
            self.set_progress(&run, RunState::Leased);
            Ok(run)
        })
    }

    fn status(&self, key: RequestKey) -> Reply<'_, Option<RequestState>> {
        self.inner.status(key)
    }

    fn progress(&self, _: ProfileId, run: RunId) -> Reply<'_, Option<RunProgress>> {
        let found = self.progress.lock().unwrap().get(&run).cloned();
        Box::pin(std::future::ready(Ok(found)))
    }

    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        self.inner.claim()
    }

    fn release_for_shutdown(&self, attempt: Attempt) -> Reply<'_, ()> {
        self.inner.release_for_shutdown(attempt)
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

    fn finish_with(&self, a: Attempt, done: Finish, outcome: RunOutcome) -> Reply<'_, Completion> {
        self.inner.finish_with(a, done, outcome)
    }

    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry> {
        self.inner.fail(attempt, failure)
    }

    fn history(&self, attempt: Attempt, limit: u32) -> Reply<'_, Vec<Turn>> {
        self.inner.history(attempt, limit)
    }
}

/// 취소 호출을 기록하고 일시 실패·동시 진입 게이트를 주입할 수 있는 시험용 `RunAdmin`.
#[derive(Default)]
pub struct FakeAdmin {
    cancels: Mutex<Vec<(ProfileId, RunId, PrincipalId, String)>>,
    failures: Mutex<VecDeque<Error>>,
    gate: Mutex<Option<Arc<Semaphore>>>,
}

impl FakeAdmin {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn cancels(&self) -> Vec<(ProfileId, RunId, PrincipalId, String)> {
        self.cancels.lock().unwrap().clone()
    }

    pub fn fail_next(&self, error: Error) {
        self.failures.lock().unwrap().push_back(error);
    }

    pub fn hold_next(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *self.gate.lock().unwrap() = Some(gate.clone());
        gate
    }
}

impl RunAdmin for FakeAdmin {
    fn resume(&self, _: ProfileId, _: RunId, _: PrincipalId, _: String) -> Reply<'_, ()> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn cancel(
        &self,
        profile: ProfileId,
        run: RunId,
        actor: PrincipalId,
        reason: String,
    ) -> Reply<'_, ()> {
        let gate = self.gate.lock().unwrap().take();
        let failure = self.failures.lock().unwrap().pop_front();
        Box::pin(async move {
            if let Some(gate) = gate {
                let _ = gate.acquire().await;
            }
            if let Some(error) = failure {
                return Err(error);
            }
            self.cancels
                .lock()
                .unwrap()
                .push((profile, run, actor, reason));
            Ok(())
        })
    }
}

pub struct UnavailableActive;

impl ActiveCheck for UnavailableActive {
    fn verify(&self) -> Reply<'_, ()> {
        Box::pin(std::future::ready(Err(Error::Unavailable)))
    }
}

pub struct UxRig {
    pub slack: FakeSlack,
    pub store: Arc<UxStore>,
    pub admin: Arc<FakeAdmin>,
    pub catalog: Arc<TestCatalog>,
    pub gateway: Arc<SlackGateway>,
    pub stop: CancellationToken,
    _serve: JoinHandle<Result<(), SlackError>>,
}

impl Drop for UxRig {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub async fn ux_rig(
    profiles: Vec<Fixture>,
    active: Option<Arc<dyn ActiveCheck>>,
    timing: Option<(Duration, Duration)>,
) -> UxRig {
    let slack = FakeSlack::start().await;
    let store = UxStore::new();
    let admin = FakeAdmin::new();
    let catalog = TestCatalog::new(profiles.into_iter().map(Fixture::build));
    let mut ingress = Ingress::new(
        catalog.clone(),
        Arc::new(host("")),
        store.clone(),
        MemoryPrivate::new(),
    );
    if let Some(active) = active {
        ingress = ingress.with_active(active);
    }
    let ingress = Arc::new(ingress);
    let base = SlackGateway::new(
        ingress,
        MemoryBroker::new(),
        MemoryLedger::new(),
        default_secrets(),
        SlackApi::new(Some(&slack.base)),
    )
    .with_threads(MemoryThreads::new());
    let gateway = match timing {
        Some((poll, limit)) => base.with_ux_timing(store.clone(), admin.clone(), poll, limit),
        None => base.with_ux(store.clone(), admin.clone()),
    };
    let gateway = Arc::new(gateway);
    let stop = CancellationToken::new();
    let serve = tokio::spawn(gateway.clone().serve(stop.clone()));
    assert!(slack.connected().await);
    UxRig {
        slack,
        store,
        admin,
        catalog,
        gateway,
        stop,
        _serve: serve,
    }
}

pub fn stop_requested_frame(
    envelope: &str,
    channel: &str,
    thread_ts: &str,
    user: Option<&str>,
) -> Value {
    let mut event = json!({
        "type": "agent_session_stopped",
        "channel": channel,
        "thread_ts": thread_ts,
    });
    if let Some(u) = user {
        event["user"] = json!(u);
    }
    json!({
        "type": "events_api",
        "envelope_id": envelope,
        "payload": {"team_id": "T1", "event_id": format!("Ev-{envelope}"), "event": event}
    })
}

pub fn thread_mention_frame(
    envelope: &str,
    channel: &str,
    user: &str,
    ts: &str,
    thread_ts: &str,
    text: &str,
) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope,
        "payload": {
            "team_id": "T1",
            "event_id": format!("Ev-{envelope}"),
            "event": {
                "type": "app_mention",
                "user": user,
                "text": text,
                "ts": ts,
                "thread_ts": thread_ts,
                "channel": channel
            }
        }
    })
}

pub fn personal_with_admin() -> Fixture {
    personal()
        .member("slack:home/U2", &["admin"])
        .role("admin", &["run"])
}
