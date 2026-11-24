//! 명시 목적지 종단 시험의 공용 장치: 프로필·호스트 설정·Slack 승인 버튼·진단.
#![allow(dead_code)]
use super::{Rig, bodies, cli_gateway};
use bot_gateway::testing::slack::{Call, FakeSlack, button_frame};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

/// 소유자는 Slack 사용자 U1(DM·팀 채널)이고 CLI 계정 `owner`로도 들어온다. 팀 채널 바인딩 `team`이 목적지로 등록된 곳이다(`planning` → C1).
pub const ROUTE_PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run, schedule.create, schedule.list, schedule.cancel]
members:
  - {identity: "slack:home/U1", roles: [owner]}
  - {identity: "local:owner", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
  - {id: dm, adapter: slack, workspace: home, dm: true, agent: main}
  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}
secrets: []
policy:
  scopes: [run, schedule.create, schedule.list, schedule.cancel]
  approve: []
"#;

pub fn route_gateway() -> String {
    format!(
        r#"{}, "slack": {{"workspaces": {{"home": {{"team_id": "T1", "bot_token": "bot-token", "app_token": "app-token", "dm": true, "channels": {{"planning": "C1"}}}}}}}}"#,
        cli_gateway()
    )
}

/// 메시지마다 다른 Slack `ts`
pub fn ts(n: u32) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{secs}.{n:06}")
}

pub const PENDING: &str = "select count(*) from {s}.bot_approvals where state = 'pending'";

/// Slack에 **성공으로 게시된** DM 승인 카드의 자리와 승인 버튼: 게시된 채널·메시지 `ts`(가짜 서버의 응답이 말한 것)·버튼 값.
pub struct PostedCard {
    pub channel: String,
    pub ts: String,
    pub value: String,
}

/// 이 승인(nonce)의 카드가 Slack에 **게시됨으로 확정될** 때까지 기다려 그 카드의 실제 채널·`ts`·승인 버튼 값을 읽는다. 운영 발송기와 같은 판정(`Call::posted`)을 쓴다 —
/// 정상 HTTP·`ok: true`·응답 채널이 요청 채널과 같음·유효한 `ts`·잘림 경고 없음. 거절되거나 어긋나거나 불명인 요청의 카드는 고르지 않는다(카드 전달 회귀를 잡는다).
pub async fn posted_card(slack: &FakeSlack, nonce: &str) -> PostedCard {
    for _ in 0..300 {
        for call in slack.calls("chat.postMessage") {
            let Some((channel, ts)) = call.posted() else {
                continue;
            };
            let blocks = call.body["blocks"].as_array().cloned().unwrap_or_default();
            let button = blocks
                .iter()
                .flat_map(|block| block["elements"].as_array().cloned().unwrap_or_default())
                .find(|element| element["action_id"] == "bot_approve" && element["value"] == nonce);
            if let (true, Some(button)) = (channel.starts_with('D'), button) {
                return PostedCard {
                    channel,
                    ts,
                    value: button["value"].as_str().unwrap().to_owned(),
                };
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("승인 카드가 Slack에 게시되지 않았어요: {nonce}");
}

/// 소유자(U1)가 게시된 카드의 승인 버튼을 누른다(Slack 소켓의 `block_actions`). 결정은 게이트웨이 → 결정 브로커 경로를 지난다.
pub async fn click(slack: &FakeSlack, envelope: &str, card: &PostedCard) {
    slack
        .push(button_frame(
            envelope,
            "T1",
            &card.channel,
            "U1",
            &card.ts,
            "bot_approve",
            &card.value,
        ))
        .await;
}

/// 열려 있는 승인 카드 하나를 소유자(U1)가 **Slack에 게시된 카드의 실제 승인 버튼**으로 승인한다.
pub async fn approve(slack: &FakeSlack, rig: &Rig, envelope: &str) {
    let nonce = rig.query("select nonce from {s}.bot_approvals where state = 'pending'");
    assert!(!nonce.is_empty() && !nonce.contains('\n'), "{nonce:?}");
    let card = posted_card(slack, &nonce).await;
    click(slack, envelope, &card).await;
}

/// 가짜 Slack이 받은 `chat.postMessage` 중 이 채널로 간 것
pub fn posts_to(slack: &FakeSlack, channel: &str) -> Vec<Call> {
    slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|call| call.body["channel"] == channel)
        .collect()
}

/// run 상태·모델 요청 요약(시험이 기다리다 실패할 때 원인을 보이는 진단)
pub fn dump(rig: &Rig) -> String {
    let requests = bodies(&rig.upstream)
        .iter()
        .map(|body| {
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            let last = messages.last().map(|m| m.to_string()).unwrap_or_default();
            format!(
                "[{}개, 끝: {}]",
                messages.len(),
                last.chars().take(200).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "감사: {} / runs: {} / 모델 요청 {}건: {requests}",
        rig.query("select string_agg(action || ':' || verdict, ',' order by seq) from {s}.bot_audit"),
        rig.query(
            "select string_agg(state || ':' || attempts || ':' || coalesce(error, ''), ' / ') from {s}.bot_runs"
        ),
        rig.upstream.requests().len()
    )
}

/// 조건이 될 때까지 기다린다(90초). 실패하면 진단과 함께 멈춘다.
pub async fn wait(rig: &Rig, what: &str, ready: impl Fn(&Rig) -> bool) {
    for _ in 0..180 {
        if ready(rig) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("{what}: {}", dump(rig));
}

pub fn create_with(destination: serde_json::Value, at: &str) -> String {
    json!({"at": at, "instruction": "자료 도착을 팀에 알려 줘", "quiet": false, "destination": destination})
        .to_string()
}

pub fn team_channel(id: &str) -> serde_json::Value {
    json!({"binding": "team", "conversation": {"kind": "channel", "id": id}})
}

/// 팀 채널 바인딩(`team`)을 뺀 같은 프로필: 운영자가 등록부에서 수신자를 뺀 새 판본
pub fn route_profile_without_team() -> String {
    ROUTE_PROFILE.replace(
        "  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}\n",
        "",
    )
}
