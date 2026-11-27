//! Shared fixtures and Slack approval helpers for explicit-destination schedule tests.
#![allow(dead_code)]
use super::{Rig, bodies, cli_gateway};
use bot_gateway::testing::slack::{Call, FakeSlack, button_frame};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

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

pub fn ts(n: u32) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{secs}.{n:06}")
}

pub const PENDING: &str = "select count(*) from {s}.bot_approvals where state = 'pending'";

pub struct PostedCard {
    pub channel: String,
    pub ts: String,
    pub value: String,
}

/// Waits until the approval card for `nonce` is confirmed posted to a Slack DM (`Call::posted`).
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
    panic!("approval card was not posted to Slack: {nonce}");
}

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

pub async fn approve(slack: &FakeSlack, rig: &Rig, envelope: &str) {
    let nonce = rig.query("select nonce from {s}.bot_approvals where state = 'pending'");
    assert!(!nonce.is_empty() && !nonce.contains('\n'), "{nonce:?}");
    let card = posted_card(slack, &nonce).await;
    click(slack, envelope, &card).await;
}

pub fn posts_to(slack: &FakeSlack, channel: &str) -> Vec<Call> {
    slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|call| call.body["channel"] == channel)
        .collect()
}

pub fn dump(rig: &Rig) -> String {
    let requests = bodies(&rig.upstream)
        .iter()
        .map(|body| {
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            let last = messages.last().map(|m| m.to_string()).unwrap_or_default();
            format!(
                "[{} msgs, last: {}]",
                messages.len(),
                last.chars().take(200).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "audit: {} / runs: {} / {} model request(s): {requests}",
        rig.query("select string_agg(action || ':' || verdict, ',' order by seq) from {s}.bot_audit"),
        rig.query(
            "select string_agg(state || ':' || attempts || ':' || coalesce(error, ''), ' / ') from {s}.bot_runs"
        ),
        rig.upstream.requests().len()
    )
}

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
    json!({"at": at, "instruction": "Notify the team when the report arrives", "quiet": false, "destination": destination})
        .to_string()
}

pub fn team_channel(id: &str) -> serde_json::Value {
    json!({"binding": "team", "conversation": {"kind": "channel", "id": id}})
}

pub fn route_profile_without_team() -> String {
    ROUTE_PROFILE.replace(
        "  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}\n",
        "",
    )
}
