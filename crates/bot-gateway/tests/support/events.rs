//! 이벤트 시험(`slack_events`·`slack_reactions`)이 함께 쓰는 도우미: 채널 바인딩 프로필·스레드 증거 키·프레임·반응 트리거.
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
        // 도구만 쓸 수 있고 에이전트는 실행하지 못하는 멤버(`run` 없음)
        .member("slack:home/U5", &["reader"])
        // 멤버로 (잘못) 올라 있는 Slackbot: 사람 판정이 멤버 확인과 별개로 거른다
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

/// 스레드 안의 `app_mention` 프레임
pub fn mention_in_thread(envelope: &str, user: &str, ts: &str, root: &str, text: &str) -> Value {
    json!({"type": "events_api", "envelope_id": envelope, "payload": {
        "team_id": "T1", "event_id": format!("Ev-{envelope}"),
        "event": {"type": "app_mention", "user": user, "text": text, "ts": ts, "thread_ts": root, "channel": "C1"}}})
}

pub fn event(envelope: &str, event: Value) -> Value {
    json!({"type": "events_api", "envelope_id": envelope, "payload": {
        "team_id": "T1", "event_id": format!("Ev-{envelope}"), "event": event}})
}

/// 프레임을 보내고 **이번에 보낸 만큼** 더 ack될 때까지 기다린다(앞서 받은 ack는 세지 않는다).
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
