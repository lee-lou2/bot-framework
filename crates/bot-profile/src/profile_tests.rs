//! 바인딩의 입구 종류 해석: 스키마를 거치지 않은 값에도 알려진 `adapter`만 입구가 되고, 모르는 값은 조용히 웹훅으로 읽지 않고 거절한다.
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
            Adapter::Http { .. } => "http",
            Adapter::OpenAi { .. } => "openai",
        })
        .collect();
    // 바인딩은 id 순서(desk · direct · gpt · inbox · team)다
    assert_eq!(
        kinds,
        ["cli", "slack-dm", "openai", "http", "slack-channel"]
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
    // `model`이 있어도 알려진 입구가 아니면 OpenAI 바인딩으로 읽지 않는다
    let (parts, issues) = parsed(json!([
        {"id": "x", "adapter": "discord", "endpoint": "hooks", "model": "m", "agent": "main"},
    ]));
    assert!(issues.has_errors() && parts.is_none());
}
