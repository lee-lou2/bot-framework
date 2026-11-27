//! Shared fixtures and helpers for owner-DM approval card e2e tests (`ops` origin channel, `team` target channel).
#![allow(dead_code, unused_imports)]
use super::{Rig, cli_gateway};
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretStore, SecretValue},
};
use bot_gateway::testing::slack::{Call, FakeSlack};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// Mutable secret store used to simulate live bot-token rotation during a run.
pub struct Rotating {
    values: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Rotating {
    pub fn new(entries: &[(&str, &str)]) -> Arc<Self> {
        let values = entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.as_bytes().to_vec()))
            .collect();
        Arc::new(Self {
            values: Arc::new(Mutex::new(values)),
        })
    }

    pub fn set(&self, name: &str, value: &str) {
        self.values
            .lock()
            .unwrap()
            .insert(name.to_owned(), value.as_bytes().to_vec());
    }
}

struct Bound(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl SecretStore for Rotating {
    fn for_profile(&self, _: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let values = self.values.clone();
        Box::pin(async move { Ok(Box::new(Bound(values)) as Box<dyn ProfileSecrets>) })
    }
}

impl ProfileSecrets for Bound {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        let found = self.0.lock().unwrap().get(name.as_str()).cloned();
        Box::pin(async move { found.map(SecretValue).ok_or(Error::Missing) })
    }
}

/// Owner is Slack user U1 with no general DM binding (only owner-DM card buttons are accepted); origin is `ops` (C2) and destination is `team` (`planning` -> C1).
pub const CARD_PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run, schedule.create, schedule.list, schedule.cancel]
members:
  - {identity: "slack:home/U1", roles: [owner]}
bindings:
  - {id: ops, adapter: slack, workspace: home, channel: ops, agent: main}
  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}
secrets: []
policy:
  scopes: [run, schedule.create, schedule.list, schedule.cancel]
  approve: []
"#;

pub fn card_gateway() -> String {
    format!(
        r#"{}, "slack": {{"workspaces": {{"home": {{"team_id": "T1", "bot_token": "bot-token", "app_token": "app-token", "channels": {{"planning": "C1", "ops": "C2"}}}}}}}}"#,
        cli_gateway()
    )
}

pub use bot_core::cards::{
    DM_SENT_NOTICE as DM_NOTICE, DM_UNAVAILABLE_NOTICE, DM_UNCONFIRMED_NOTICE,
};

pub fn said_in(slack: &FakeSlack, channel: &str, text: &str) -> Vec<Call> {
    slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|call| call.body["channel"] == channel && call.body["text"] == text)
        .collect()
}

pub fn texts_to(slack: &FakeSlack, channel: &str) -> Vec<String> {
    slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|call| call.body["channel"] == channel)
        .map(|call| call.body["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

pub fn has_buttons(call: &Call) -> bool {
    call.body["blocks"]
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "actions"))
}

pub fn pending_nonce(rig: &Rig) -> String {
    let nonce = rig.query("select nonce from {s}.bot_approvals where state = 'pending'");
    assert!(!nonce.is_empty() && !nonce.contains('\n'), "{nonce:?}");
    nonce
}

pub fn job_state(rig: &Rig, nonce: &str) -> String {
    rig.query(&format!(
        "select state from {{s}}.bot_card_jobs where nonce = '{nonce}'"
    ))
}
