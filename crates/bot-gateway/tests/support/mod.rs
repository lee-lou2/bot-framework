//! Slack 시험 공용 조립: 가짜 Slack 서버·메모리 포트·합성 프로필.
#![allow(dead_code)]
pub mod events;

use bot_core::profile::Adapter;
use bot_gateway::{
    active::ActiveCheck,
    host::HostConfig,
    ingress::Ingress,
    slack::{SlackError, SlackGateway, api::SlackApi},
    testing::{
        ports::{MemoryBroker, MemoryLedger, MemoryThreads},
        slack::FakeSlack,
        *,
    },
};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub fn host(extra: &str) -> HostConfig {
    HostConfig::from_json(&format!(
        r#"{{"slack": {{"workspaces": {{"home": {{"team_id": "T1", "app_id": "A1", "dm": true,
            "bot_token": "bot-token", "app_token": "app-token",
            "channels": {{"planning": "C1", "ops": "C2"}}}}}}}}, "limits": {{"per_principal_per_min": 50}}{extra}}}"#
    ))
    .unwrap()
}

pub fn personal() -> Fixture {
    profile("personal")
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .binding(
            "direct",
            Adapter::SlackDm {
                workspace: "home".into(),
            },
            "main",
        )
        .member("slack:home/U1", &["owner"])
        .role("owner", &["run"])
        .policy_scopes(&["run"])
}

pub fn work() -> Fixture {
    profile("work")
        .binding(
            "ops",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "ops".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .member("slack:home/U1", &["owner"])
        .role("owner", &["run"])
        .policy_scopes(&["run"])
}

pub fn secrets(entries: &[(&str, &str, &str)]) -> Arc<MemorySecrets> {
    let owned: Vec<(&str, &str, &[u8])> = entries
        .iter()
        .map(|(p, n, v)| (*p, *n, v.as_bytes()))
        .collect();
    MemorySecrets::new(&owned)
}

pub struct Rig {
    pub slack: FakeSlack,
    pub store: Arc<MemoryStore>,
    pub private: Arc<MemoryPrivate>,
    pub broker: Arc<MemoryBroker>,
    pub ledger: Arc<MemoryLedger>,
    pub threads: Arc<MemoryThreads>,
    pub gateway: Arc<SlackGateway>,
    pub ingress: Arc<Ingress>,
    pub stop: CancellationToken,
    pub serve: JoinHandle<Result<(), SlackError>>,
    pub tracer: Arc<RecordingTracer>,
    pub catalog: Arc<TestCatalog>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub async fn rig_with(
    profiles: Vec<Fixture>,
    secrets: Arc<MemorySecrets>,
    config: HostConfig,
    tune: impl FnOnce(&FakeSlack),
) -> Rig {
    rig_checked(profiles, secrets, config, None, tune).await
}

/// 저장소의 활성 집합과 견주는 확인(`active`)을 붙여 조립한다. None이면 확인하지 않는다.
pub async fn rig_checked(
    profiles: Vec<Fixture>,
    secrets: Arc<MemorySecrets>,
    config: HostConfig,
    active: Option<Arc<dyn ActiveCheck>>,
    tune: impl FnOnce(&FakeSlack),
) -> Rig {
    rig_full(profiles, secrets, config, active, None, tune).await
}

/// 봇 토큰을 보관소에서 다시 읽는 간격(`ttl`)까지 정해 조립한다.
pub async fn rig_full(
    profiles: Vec<Fixture>,
    secrets: Arc<MemorySecrets>,
    config: HostConfig,
    active: Option<Arc<dyn ActiveCheck>>,
    ttl: Option<Duration>,
    tune: impl FnOnce(&FakeSlack),
) -> Rig {
    let slack = FakeSlack::start().await;
    tune(&slack);
    let store = MemoryStore::new();
    let private = MemoryPrivate::new();
    let broker = MemoryBroker::new();
    let ledger = MemoryLedger::new();
    let tracer = RecordingTracer::new();
    let catalog = TestCatalog::new(profiles.into_iter().map(Fixture::build));
    let mut ingress = Ingress::new(
        catalog.clone(),
        Arc::new(config),
        store.clone(),
        private.clone(),
    )
    .with_tracer(tracer.clone());
    if let Some(active) = active {
        ingress = ingress.with_active(active);
    }
    let ingress = Arc::new(ingress);
    let threads = MemoryThreads::new();
    let mut gateway = SlackGateway::new(
        ingress.clone(),
        broker.clone(),
        ledger.clone(),
        secrets,
        SlackApi::new(Some(&slack.base)),
    )
    .with_threads(threads.clone());
    if let Some(ttl) = ttl {
        gateway = gateway.with_token_ttl(ttl);
    }
    let gateway = Arc::new(gateway);
    let stop = CancellationToken::new();
    let serve = tokio::spawn(gateway.clone().serve(stop.clone()));
    Rig {
        slack,
        store,
        private,
        broker,
        ledger,
        threads,
        gateway,
        ingress,
        stop,
        serve,
        tracer,
        catalog,
    }
}

pub async fn rig(
    profiles: Vec<Fixture>,
    secrets: Arc<MemorySecrets>,
    tune: impl FnOnce(&FakeSlack),
) -> Rig {
    rig_with(profiles, secrets, host(""), tune).await
}

pub fn default_secrets() -> Arc<MemorySecrets> {
    secrets(&[
        ("personal", "bot-token", "test-bot-1"),
        ("personal", "app-token", "test-app-1"),
    ])
}

pub async fn connected(profiles: Vec<Fixture>) -> Rig {
    let rig = rig(profiles, default_secrets(), |_| {}).await;
    assert!(rig.slack.connected().await);
    rig
}

/// 기동 검사가 실패해 `serve`가 돌아온 오류
pub async fn startup_failure(rig: &mut Rig) -> SlackError {
    tokio::time::timeout(Duration::from_secs(5), &mut rig.serve)
        .await
        .expect("기동 검사가 끝나지 않았어요")
        .unwrap()
        .unwrap_err()
}

pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}
