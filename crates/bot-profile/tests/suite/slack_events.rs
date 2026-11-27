//! Slack 채널 바인딩의 `events` 선택: 선언 규칙, 기본값, 트리거 사건과의 대조, 판본에 들어가는지.

use crate::support::{Files, append, base, codes, edit, snapshot};
use bot_core::{id::BindingId, profile::Adapter, slack::SlackEvent};
use std::collections::BTreeSet;

const TEAM: &str = "{id: team, adapter: slack, workspace: home, channel: planning, agent: main}";
const DIRECT: &str = "{id: direct, adapter: slack, workspace: home, dm: true, agent: main}";
const TRIGGERS: &str = "agents/main/triggers.yaml";

fn channel_with(declaration: &str) -> Files {
    edit(
        base(),
        "profile.yaml",
        TEAM,
        &format!(
            "{{id: team, adapter: slack, workspace: home, channel: planning, agent: main{declaration}}}"
        ),
    )
}

fn events_of(files: &Files) -> BTreeSet<SlackEvent> {
    match &snapshot(files).bindings[&BindingId("team".into())].source {
        Adapter::SlackChannel { events, .. } => events.clone(),
        other => panic!("채널 바인딩이어야 해요: {other:?}"),
    }
}

fn trigger(on: &str) -> String {
    format!("  - id: chat\n    \"on\": {on}\n    target: {{instruction: x}}\n")
}

#[test]
fn the_default_is_mentions_only_and_a_declared_set_is_kept_as_written() {
    assert_eq!(events_of(&base()), BTreeSet::from([SlackEvent::Mention]));
    assert_eq!(
        events_of(&channel_with(", events: [reaction, thread_reply]")),
        BTreeSet::from([SlackEvent::ThreadReply, SlackEvent::Reaction]),
        "명시하면 그 집합 그대로다(멘션은 자동으로 더하지 않는다)"
    );
    assert_eq!(
        events_of(&channel_with(", events: [mention, thread_reply, reaction]")).len(),
        3
    );
    // DM 바인딩은 `events: [dm]`만 허용한다(생략과 같다)
    let dm = edit(
        base(),
        "profile.yaml",
        DIRECT,
        "{id: direct, adapter: slack, workspace: home, dm: true, agent: main, events: [dm]}",
    );
    assert!(codes(&dm).is_empty(), "{:?}", codes(&dm));
}

#[test]
fn malformed_declarations_are_refused_before_anything_is_activated() {
    let channel = |d: &str| codes(&channel_with(d));
    for (case, declaration) in [
        ("빈 목록", ", events: []"),
        ("중복", ", events: [mention, mention]"),
        ("채널에 dm", ", events: [dm]"),
        ("모르는 값", ", events: [typing]"),
        ("목록이 아님", ", events: mention"),
    ] {
        assert_eq!(channel(declaration), ["schema"], "{case}");
    }
    // DM 바인딩에는 dm만, HTTP·CLI 바인딩에는 events 자체가 없다
    for (case, from, to) in [
        (
            "DM에 mention",
            DIRECT,
            "{id: direct, adapter: slack, workspace: home, dm: true, agent: main, events: [mention]}",
        ),
        (
            "HTTP에 events",
            "{id: inbox, adapter: http, endpoint: reminders, agent: main}",
            "{id: inbox, adapter: http, endpoint: reminders, agent: main, events: [mention]}",
        ),
        (
            "CLI에 events",
            "{id: desk, adapter: cli, account: owner, agent: main}",
            "{id: desk, adapter: cli, account: owner, agent: main, events: [mention]}",
        ),
    ] {
        assert_eq!(
            codes(&edit(base(), "profile.yaml", from, to)),
            ["schema"],
            "{case}"
        );
    }
}

#[test]
fn a_trigger_must_be_on_a_binding_that_receives_its_event() {
    // 기본 바인딩(멘션만)에는 스레드 답글·반응 트리거를 달 수 없다
    for event in ["thread-reply", "reaction"] {
        let files = append(
            base(),
            TRIGGERS,
            &trigger(&format!("{{event: {event}, binding: team}}")),
        );
        assert_eq!(codes(&files), ["event-not-enabled"], "{event}");
    }
    // 바인딩이 그 종류를 받으면 통과한다
    let both = append(
        append(
            channel_with(", events: [mention, thread_reply, reaction]"),
            TRIGGERS,
            &trigger("{event: thread-reply, binding: team}"),
        ),
        TRIGGERS,
        "  - id: ack\n    \"on\": {event: reaction, binding: team}\n    target: {instruction: y}\n",
    );
    assert!(codes(&both).is_empty(), "{:?}", codes(&both));
    // 멘션을 빼면 `message` 트리거가 영영 불리지 않으니 거절한다
    let no_mention = append(
        channel_with(", events: [reaction]"),
        TRIGGERS,
        &trigger("{event: message, binding: team}"),
    );
    assert_eq!(codes(&no_mention), ["event-not-enabled"]);
    // 채널 바인딩이 아닌 곳에는 스레드 답글·반응 트리거를 달 수 없다
    for event in ["thread-reply", "reaction"] {
        for binding in ["direct", "inbox", "desk"] {
            let files = append(
                base(),
                TRIGGERS,
                &trigger(&format!("{{event: {event}, binding: {binding}}}")),
            );
            assert_eq!(
                codes(&files),
                ["binding-kind-mismatch"],
                "{event}/{binding}"
            );
        }
    }
}

#[test]
fn changing_the_declared_events_changes_the_activation_revision() {
    let revision = |files: &Files| snapshot(files).revision.0;
    let plain = revision(&base());
    let thread = revision(&channel_with(", events: [mention, thread_reply]"));
    let reaction = revision(&channel_with(", events: [mention, reaction]"));
    assert_ne!(plain, thread);
    assert_ne!(thread, reaction);
    assert_eq!(
        thread,
        revision(&channel_with(", events: [mention, thread_reply]")),
        "같은 선언은 같은 판본이다"
    );
}
