//! Tests for Slack channel binding `events` declarations, defaults, trigger compatibility, and revision binding.

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
        other => panic!("expected SlackChannel binding: {other:?}"),
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
        "explicit events list must not auto-add mention"
    );
    assert_eq!(
        events_of(&channel_with(", events: [mention, thread_reply, reaction]")).len(),
        3
    );
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
        ("empty list", ", events: []"),
        ("duplicate", ", events: [mention, mention]"),
        ("dm on channel", ", events: [dm]"),
        ("unknown value", ", events: [typing]"),
        ("not a list", ", events: mention"),
    ] {
        assert_eq!(channel(declaration), ["schema"], "{case}");
    }
    for (case, from, to) in [
        (
            "mention on DM",
            DIRECT,
            "{id: direct, adapter: slack, workspace: home, dm: true, agent: main, events: [mention]}",
        ),
        (
            "events on HTTP",
            "{id: inbox, adapter: http, endpoint: reminders, agent: main}",
            "{id: inbox, adapter: http, endpoint: reminders, agent: main, events: [mention]}",
        ),
        (
            "events on CLI",
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
    for event in ["thread-reply", "reaction"] {
        let files = append(
            base(),
            TRIGGERS,
            &trigger(&format!("{{event: {event}, binding: team}}")),
        );
        assert_eq!(codes(&files), ["event-not-enabled"], "{event}");
    }
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
    let no_mention = append(
        channel_with(", events: [reaction]"),
        TRIGGERS,
        &trigger("{event: message, binding: team}"),
    );
    assert_eq!(codes(&no_mention), ["event-not-enabled"]);
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
        "identical declaration yields identical revision"
    );
}
