//! Shared reply delivery test harness: fake Slack, memory outbox, and `Deliverer`.
#![allow(dead_code)]
use bot_core::state::{Conversation, ConversationKind};
use bot_gateway::{
    delivery::Deliverer,
    host::HostConfig,
    ingress::Ingress,
    slack::{SlackGateway, api::SlackApi},
    testing::{
        ports::{MemoryBroker, MemoryLedger, MemoryOutbox, MemoryThreads, outgoing},
        slack::FakeSlack,
        *,
    },
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

use crate::support::{default_secrets, personal};

pub const THREAD: &str = "1700000001.000100";

pub struct Rig {
    pub slack: FakeSlack,
    pub outbox: Arc<MemoryOutbox>,
    pub deliverer: Arc<Deliverer>,
    pub catalog: Arc<TestCatalog>,
    /// Recorded bot-participated thread evidence (confirmed posts are logged here).
    pub threads: Arc<MemoryThreads>,
    gateway: Arc<SlackGateway>,
    ingress: Arc<Ingress>,
}

impl Rig {
    pub fn card_deliverer(
        &self,
        jobs: Arc<dyn bot_gateway::slack::cards::CardOutbox>,
    ) -> bot_gateway::slack::cards::CardDeliverer {
        bot_gateway::slack::cards::CardDeliverer::new(jobs, self.ingress.clone(), self.sender())
    }

    pub fn sender(&self) -> bot_gateway::slack::send::SlackSender {
        self.gateway.sender()
    }
}

pub async fn rig() -> Rig {
    rig_with(personal(), default_secrets(), None).await
}

pub const HOST: &str = r#"{"slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token",
    "app_token": "app-token", "channels": {"planning": "C1", "ops": "C2"}}}},
    "admin_url": "https://admin.example/approvals"}"#;

pub async fn rig_with(profile: Fixture, secrets: Arc<MemorySecrets>, base: Option<&str>) -> Rig {
    rig_on(HOST, profile, secrets, base).await
}

/// Harness with a custom host registry for pre-send verification tests.
pub async fn rig_on(
    host: &str,
    profile: Fixture,
    secrets: Arc<MemorySecrets>,
    base: Option<&str>,
) -> Rig {
    build(host, profile, secrets, base, None).await
}

/// Harness with a reduced bot-token cache TTL for token-rotation tests.
pub async fn rig_ttl(secrets: Arc<MemorySecrets>, ttl: Duration) -> Rig {
    build(HOST, personal(), secrets, None, Some(ttl)).await
}

async fn build(
    host: &str,
    profile: Fixture,
    secrets: Arc<MemorySecrets>,
    base: Option<&str>,
    ttl: Option<Duration>,
) -> Rig {
    let slack = FakeSlack::start().await;
    let outbox = MemoryOutbox::new();
    let catalog = TestCatalog::new([profile.build()]);
    let host = HostConfig::from_json(host).unwrap();
    let ingress = Arc::new(Ingress::new(
        catalog.clone(),
        Arc::new(host),
        MemoryStore::new(),
        MemoryPrivate::new(),
    ));
    let api = SlackApi::new(Some(base.unwrap_or(&slack.base)));
    let threads = MemoryThreads::new();
    let mut gateway = SlackGateway::new(
        ingress.clone(),
        MemoryBroker::new(),
        MemoryLedger::new(),
        secrets,
        api,
    )
    .with_threads(threads.clone());
    if let Some(ttl) = ttl {
        gateway = gateway.with_token_ttl(ttl);
    }
    let deliverer = Arc::new(Deliverer::new(
        outbox.clone(),
        ingress.clone(),
        Arc::new(gateway.sender()),
    ));
    Rig {
        slack,
        outbox,
        deliverer,
        catalog,
        threads,
        gateway: Arc::new(gateway),
        ingress,
    }
}

pub fn channel(id: &str) -> Conversation {
    Conversation {
        kind: ConversationKind::Channel,
        id: id.into(),
        thread: Some(THREAD.into()),
    }
}

pub fn direct(id: &str) -> Conversation {
    Conversation {
        kind: ConversationKind::Direct,
        id: id.into(),
        thread: Some(THREAD.into()),
    }
}

pub fn to_channel(rig: &Rig, seq: u32, body: Value) {
    rig.outbox.push(outgoing(
        "personal",
        seq,
        "team",
        "slack:home/U1",
        channel("C1"),
        &body.to_string(),
    ));
}

pub fn settled(rig: &Rig) -> Vec<String> {
    rig.outbox
        .settled()
        .into_iter()
        .map(|(_, text)| text)
        .collect()
}

pub fn posts(rig: &Rig) -> Vec<bot_gateway::testing::slack::Call> {
    rig.slack.calls("chat.postMessage")
}
