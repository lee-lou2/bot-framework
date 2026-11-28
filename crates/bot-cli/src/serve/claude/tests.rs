use super::*;
use crate::host;
use bot_core::{
    auth::{AuthSource, CredentialKind, HeaderMode},
    id::SecretName,
};

fn parsed(extra: &str) -> HostFile {
    host::parse(&format!(
        r#"{{"version": 1, "models": {{"personal": ["assistant", "plain"], "work": ["other"]}}{extra}}}"#
    ))
    .unwrap()
}

#[test]
fn the_upstream_endpoint_is_split_into_target_and_path() {
    let (target, path) = upstream_target(CliKind::Claude, "https://api.example").unwrap();
    assert_eq!((target.host.as_str(), target.port), ("api.example", 443));
    assert_eq!(path, "/v1/messages");
    let (target, path) = upstream_target(
        CliKind::Claude,
        "https://api.example:8443/proxy/v1/messages",
    )
    .unwrap();
    assert_eq!((target.host.as_str(), target.port), ("api.example", 8443));
    assert_eq!(path, "/proxy/v1/messages");
    assert_eq!(
        upstream_target(CliKind::Claude, "https://api.example/")
            .unwrap()
            .1,
        CLAUDE_DEFAULT_PATH
    );
    assert_eq!(
        upstream_target(CliKind::Codex, "https://api.openai.com/v1")
            .unwrap()
            .1,
        "/v1/responses"
    );
    assert_eq!(
        upstream_target(CliKind::Codex, "https://api.openai.com")
            .unwrap()
            .1,
        "/v1/responses"
    );
    assert_eq!(
        upstream_target(CliKind::Codex, "https://chatgpt.com/backend-api/codex")
            .unwrap()
            .1,
        "/backend-api/codex/responses"
    );
    assert_eq!(
        upstream_target(
            CliKind::Codex,
            "https://chatgpt.com/backend-api/codex/responses"
        )
        .unwrap()
        .1,
        "/backend-api/codex/responses"
    );
    assert_eq!(
        upstream_target(CliKind::Agy, "https://generativelanguage.googleapis.com")
            .unwrap()
            .1,
        "/v1beta"
    );
    assert_eq!(
        upstream_target(
            CliKind::Agy,
            "https://generativelanguage.googleapis.com/v1beta/models"
        )
        .unwrap()
        .1,
        "/v1beta"
    );
    assert!(upstream_target(CliKind::Claude, "http://api.example").is_none());
    assert!(upstream_target(CliKind::Claude, "https://api.example:99999").is_none());
}

#[test]
fn cli_aliases_become_protocol_pinned_upstream_providers_per_profile() {
    let host = host::parse(
        r#"{"version": 1, "models": {"personal": ["assistant", "codex", "agy", "sub-claude", "plain"], "work": ["other"]},
            "providers": {
                "assistant": {"kind": "claude-cli", "model": "model-large", "endpoint": "https://api.example", "secret": "model-key"},
                "sub-claude": {"kind": "claude-cli", "model": "model-sub", "endpoint": "https://api.anthropic.com", "auth": {"kind": "subscription-token", "credential": "claude-sub"}, "max_tokens": 16384},
                "codex": {"kind": "codex-cli", "model": "gpt-5", "endpoint": "https://api.openai.com/v1", "secret": "openai-key"},
                "agy": {"kind": "agy-cli", "model": "gemini-3.1-pro-preview", "endpoint": "https://generativelanguage.googleapis.com", "secret": "gemini-key"},
                "plain": {"kind": "demo"},
                "other": {"kind": "claude-cli", "model": "model-small", "endpoint": "https://api.example:8443/v1/messages", "secret": "other-key"}}}"#,
    )
    .unwrap();
    assert!(needed(&host));
    let table = model_host(&host).unwrap();
    assert_eq!(
        table.providers.keys().cloned().collect::<Vec<_>>(),
        ["agy", "assistant", "codex", "other", "sub-claude"]
    );
    let assistant = &table.providers["assistant"];
    assert_eq!(assistant.protocol, Protocol::Anthropic);
    assert_eq!(assistant.models["assistant"], "model-large");
    assert_eq!(
        assistant.auth,
        AuthSource::Secret(SecretName::parse("model-key").unwrap())
    );
    assert_eq!(assistant.header_mode, HeaderMode::ApiKey("x-api-key"));
    let sub = &table.providers["sub-claude"];
    assert_eq!(
        sub.auth,
        AuthSource::Credential {
            name: SecretName::parse("claude-sub").unwrap(),
            expected_kind: CredentialKind::SubscriptionToken,
        }
    );
    assert_eq!(
        sub.header_mode,
        HeaderMode::BearerMergeHeader {
            header: "anthropic-beta",
            token: "oauth-2025-04-20",
        }
    );
    let codex = &table.providers["codex"];
    assert_eq!(codex.protocol, Protocol::OpenAiResponses);
    assert_eq!(codex.path, "/v1/responses");
    assert_eq!(codex.header_mode, HeaderMode::Bearer);
    assert!(codex.forward_headers.contains(&"originator".to_owned()));
    assert!(codex.forward_headers.contains(&"user-agent".to_owned()));
    let agy = &table.providers["agy"];
    assert_eq!(agy.protocol, Protocol::Gemini);
    assert_eq!(agy.path, "/v1beta");
    assert_eq!(agy.header_mode, HeaderMode::ApiKey("x-goog-api-key"));
    let personal = &table.allow[&ProfileId("personal".into())];
    assert_eq!(
        personal.iter().cloned().collect::<Vec<_>>(),
        ["agy", "assistant", "codex", "sub-claude"]
    );
    let work = &table.allow[&ProfileId("work".into())];
    assert_eq!(work.iter().cloned().collect::<Vec<_>>(), ["other"]);
}

#[test]
fn a_host_without_claude_aliases_does_not_need_the_front() {
    let demo =
        parsed(r#", "providers": {"assistant": {"kind": "demo"}, "plain": {"kind": "demo"}}"#);
    assert!(!needed(&demo));
    assert!(model_host(&demo).unwrap().providers.is_empty());
    let unused = parsed(
        r#", "providers": {"assistant": {"kind": "demo"}, "plain": {"kind": "demo"},
            "spare": {"kind": "claude-cli", "model": "m", "endpoint": "https://api.example", "secret": "k"}}"#,
    );
    assert!(!needed(&unused));
}

#[test]
fn the_park_notice_counts_by_reason() {
    let park = HostPark::default();
    let attempt = Attempt {
        profile: ProfileId("personal".into()),
        run: bot_core::id::RunId("run-1".into()),
        claim: None,
        generation: bot_core::id::Generation(1),
    };
    park.parked(&attempt, ParkReason::Approval);
    park.parked(&attempt, ParkReason::Child);
    park.parked(&attempt, ParkReason::Child);
    assert_eq!(park.counts(), (1, 0, 2));
    assert_eq!(
        park.recent().iter().map(|(_, r)| *r).collect::<Vec<_>>(),
        [ParkReason::Approval, ParkReason::Child, ParkReason::Child]
    );
    for _ in 0..40 {
        park.parked(&attempt, ParkReason::Held);
    }
    assert_eq!(park.recent().len(), 32);
    assert_eq!(park.counts().1, 40);
}
