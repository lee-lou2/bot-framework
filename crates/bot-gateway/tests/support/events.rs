//! Shared helpers for Slack event and reaction integration tests.
#![allow(dead_code)]
use super::*;
use bot_core::{
    profile::Adapter,
    slack::{BotThread, SlackEvent},
};
use bot_gateway::testing::*;
use serde_json::{Value, json};

pub const ALL: [SlackEvent; 3] = [
    SlackEvent::Mention,
    SlackEvent::ThreadReply,
    SlackEvent::Reaction,
];
pub const TC1: (&str, &str) = ("T1", "C1");

pub fn channel_profile(events: &[SlackEvent]) -> Fixture {
    profile("personal")
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
                events: events.iter().copied().collect(),
            },
            "main",
        )
        .member("slack:home/U1", &["owner"])
        .member("slack:home/U3", &["owner"])
        // Tool-only member without `run` scope.
        .member("slack:home/U5", &["reader"])
        // Slackbot registered as a member to verify human check runs independently.
        .member("slack:home/USLACKBOT", &["owner"])
        .role("owner", &["run"])
        .role("reader", &["agenda.read"])
        .policy_scopes(&["run", "agenda.read"])
}

pub fn thread(root: &str) -> BotThread {
    BotThread {
        profile: bot_core::id::ProfileId("personal".into()),
        binding: bot_core::id::BindingId("team".into()),
        team: "T1".into(),
        bot: "UBOT".into(),
        channel: "C1".into(),
        root_ts: root.into(),
    }
}

pub fn mention_in_thread(envelope: &str, user: &str, ts: &str, root: &str, text: &str) -> Value {
    json!({"type": "events_api", "envelope_id": envelope, "payload": {
        "team_id": "T1", "event_id": format!("Ev-{envelope}"),
        "event": {"type": "app_mention", "user": user, "text": text, "ts": ts, "thread_ts": root, "channel": "C1"}}})
}

pub fn event(envelope: &str, event: Value) -> Value {
    json!({"type": "events_api", "envelope_id": envelope, "payload": {
        "team_id": "T1", "event_id": format!("Ev-{envelope}"), "event": event}})
}

/// Pushes frames and waits for that many additional acks beyond prior ones.
pub async fn push_all(rig: &Rig, frames: Vec<Value>) {
    let want = rig.slack.acks().len() + frames.len();
    for frame in frames {
        rig.slack.push(frame).await;
    }
    assert!(
        rig.slack.until(|s| s.acks().len() >= want).await,
        "{:?}",
        rig.slack.acks()
    );
}

pub fn reaction_trigger(id: &str, order: i64, tool: &str) -> bot_core::profile::Trigger {
    let mut trigger = event_trigger_on(id, "team", "reaction", tool_target(tool, "{}"));
    trigger.order = order;
    trigger.conditions.insert(
        "body.reaction".into(),
        bot_core::command::JsonPayload("\"white_check_mark\"".into()),
    );
    trigger
}
