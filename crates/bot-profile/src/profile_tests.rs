use super::*;
use serde_json::json;

fn parsed(bindings: Value) -> (Option<ProfileParts>, Issues) {
    let mut issues = Issues::new(50);
    let parts = parse(
        &json!({"id": "p", "bindings": bindings}),
        None,
        &Limits::default(),
        &mut issues,
    );
    (parts, issues)
}

#[test]
fn every_known_adapter_becomes_its_own_entrance() {
    let (parts, issues) = parsed(json!([
        {"id": "desk", "adapter": "cli", "account": "owner", "agent": "main"},
        {"id": "team", "adapter": "slack", "workspace": "home", "channel": "planning", "agent": "main"},
        {"id": "direct", "adapter": "slack", "workspace": "home", "dm": true, "agent": "main"},
        {"id": "tg", "adapter": "telegram", "bot": "main", "dm": true, "agent": "main"},
        {"id": "tg-groups", "adapter": "telegram", "bot": "main", "groups": true, "agent": "main"},
        {"id": "inbox", "adapter": "http", "endpoint": "hooks", "agent": "main"},
        {"id": "gpt", "adapter": "http", "endpoint": "openai", "model": "assistant", "agent": "main"},
    ]));
    assert!(!issues.has_errors());
    let kinds: Vec<&str> = parts
        .unwrap()
        .bindings
        .values()
        .map(|binding| match &binding.kind {
            Adapter::Cli { .. } => "cli",
            Adapter::SlackChannel { .. } => "slack-channel",
            Adapter::SlackDm { .. } => "slack-dm",
            Adapter::TelegramDm { .. } => "telegram-dm",
            Adapter::TelegramGroups { .. } => "telegram-groups",
            Adapter::Http { .. } => "http",
            Adapter::OpenAi { .. } => "openai",
        })
        .collect();
    // Ordered by binding id: desk, direct, gpt, inbox, team, tg, tg-groups.
    assert_eq!(
        kinds,
        [
            "cli",
            "slack-dm",
            "openai",
            "http",
            "slack-channel",
            "telegram-dm",
            "telegram-groups"
        ]
    );
}

#[test]
fn an_unknown_adapter_is_refused_by_name_and_never_read_as_a_webhook() {
    for adapter in ["discord", "", "HTTP", "slack "] {
        let (parts, issues) = parsed(json!([
            {"id": "x", "adapter": adapter, "endpoint": "hooks", "agent": "main"},
        ]));
        assert!(issues.has_errors(), "{adapter:?}");
        assert!(parts.is_none(), "{adapter:?}");
    }
    let (parts, issues) = parsed(json!([
        {"id": "x", "adapter": "discord", "endpoint": "hooks", "model": "m", "agent": "main"},
    ]));
    assert!(issues.has_errors() && parts.is_none());
}
