//! 소유자 DM 승인 카드 종단 시험의 공용 장치: 원래 채널(`ops`)·목적지 채널(`team`) 프로필과 DB 확인.
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

/// 값을 바꿀 수 있는 보관소(실행 중인 토큰 교체를 흉내 낸다): 어떤 프로필로 열어도 같은 이름·값을 주고 `set`으로 한 이름의 값을 바꾼다.
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

    /// 보관소의 이 이름의 값을 바꾼다(키 교체)
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

/// 소유자는 Slack 사용자 U1이다. 일반 DM 입력 바인딩은 **없고**(소유자 DM으로 가는 승인 카드의 버튼만 처리해야 한다), 예약을 시작하는 원래 채널은 `ops`(C2),
/// 명시 목적지로 등록된 채널은 `team`(`planning` → C1)이다.
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

/// 원래 채널·스레드에 가는 공개 한 줄: 카드를 DM으로 보냈다 / DM으로 보낼 수 없어 닫았다 / DM으로 전달됐는지 확인하지 못해 닫았다(글은 core 상수 하나가 정한다)
pub use bot_core::cards::{
    DM_SENT_NOTICE as DM_NOTICE, DM_UNAVAILABLE_NOTICE, DM_UNCONFIRMED_NOTICE,
};

/// 이 채널로 간 `chat.postMessage` 중 본문 글이 정확히 `text`인 것
pub fn said_in(slack: &FakeSlack, channel: &str, text: &str) -> Vec<Call> {
    slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|call| call.body["channel"] == channel && call.body["text"] == text)
        .collect()
}

/// 가짜 Slack이 받은 `chat.postMessage` 중 이 채널로 간 것의 본문 글
pub fn texts_to(slack: &FakeSlack, channel: &str) -> Vec<String> {
    slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|call| call.body["channel"] == channel)
        .map(|call| call.body["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// 승인 버튼(`actions` 블록)이 달린 게시
pub fn has_buttons(call: &Call) -> bool {
    call.body["blocks"]
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "actions"))
}

/// 열려 있는 승인(`pending`)의 식별자 하나
pub fn pending_nonce(rig: &Rig) -> String {
    let nonce = rig.query("select nonce from {s}.bot_approvals where state = 'pending'");
    assert!(!nonce.is_empty() && !nonce.contains('\n'), "{nonce:?}");
    nonce
}

/// 이 승인의 전달 작업 상태(`staged`·`leased`·`sending`·`posted`·`closed`)
pub fn job_state(rig: &Rig, nonce: &str) -> String {
    rig.query(&format!(
        "select state from {{s}}.bot_card_jobs where nonce = '{nonce}'"
    ))
}
