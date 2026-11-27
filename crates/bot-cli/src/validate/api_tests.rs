use super::*;
use crate::host;
use bot_gateway::testing::profile;

fn host_with(providers: &str) -> HostFile {
    host::parse(&format!(
        r#"{{"version": 1, "models": {{"personal": ["assistant", "plain"], "work": ["other"]}}{providers}}}"#
    ))
    .unwrap()
}

#[test]
fn the_api_aliases_need_their_keys_but_never_the_model_vm_and_a_keyless_local_server_needs_neither()
{
    let snapshot = profile("personal").build();
    let host = host_with(
        r#", "providers": {
            "assistant": {"kind": "anthropic-api", "model": "m", "endpoint": "https://api.example", "secret": "anthropic-key"},
            "plain": {"kind": "openai-api", "model": "m", "endpoint": "http://127.0.0.1:11434/v1"},
            "other": {"kind": "openai-api", "model": "m", "endpoint": "https://api.example/v1", "secret": "work-key"}}"#,
    );
    assert_eq!(
        model_secrets(&snapshot, &host),
        BTreeSet::from(["anthropic-key".to_owned()])
    );
    let needed = needed_secrets(&snapshot, &host);
    assert!(
        needed.contains("anthropic-key") && !needed.contains("work-key"),
        "{needed:?}"
    );
    assert!(missing_model_sandbox(&snapshot, &host).is_none());
    let mixed = host_with(
        r#", "providers": {
            "assistant": {"kind": "claude-cli", "model": "m", "endpoint": "https://api.example", "secret": "model-key"},
            "plain": {"kind": "openai-api", "model": "m", "endpoint": "https://api.example/v1", "secret": "openai-key"}}"#,
    );
    assert!(missing_model_sandbox(&snapshot, &mixed).is_some());
    assert_eq!(
        model_secrets(&snapshot, &mixed),
        BTreeSet::from(["model-key".to_owned(), "openai-key".to_owned()])
    );
}

fn snapshot_with_builtin(model: &str, builtins: &[Builtin]) -> Snapshot {
    let mut snap = (*profile("personal").build()).clone();
    let main = snap
        .agents
        .get_mut(&bot_core::id::AgentId("main".into()))
        .unwrap();
    main.model = model.to_owned();
    main.builtin = builtins.iter().copied().collect();
    snap
}

#[test]
fn agent_builtin_tools_are_checked_against_the_configured_provider_kind() {
    let host = host_with(
        r#", "providers": {
            "api": {"kind": "anthropic-api", "model": "m", "endpoint": "https://api.example", "secret": "a-key"},
            "openai": {"kind": "openai-api", "model": "m", "endpoint": "http://127.0.0.1:11434/v1"},
            "cli": {"kind": "claude-cli", "model": "m", "endpoint": "https://api.example", "secret": "c-key"},
            "codex": {"kind": "codex-cli", "model": "gpt-5", "endpoint": "https://api.openai.com/v1", "secret": "o-key"},
            "agy": {"kind": "agy-cli", "model": "gemini-3.1-pro-preview", "endpoint": "https://generativelanguage.googleapis.com", "secret": "g-key"},
            "plain": {"kind": "demo"}}"#,
    );
    for alias in ["api", "openai"] {
        assert!(unsupported_builtin_tools(&snapshot_with_builtin(alias, &[]), &host).is_empty());
        let probs =
            unsupported_builtin_tools(&snapshot_with_builtin(alias, &[Builtin::Read]), &host);
        assert_eq!(probs.len(), 1, "{alias}: {probs:?}");
        assert!(
            probs[0].contains("read") && probs[0].contains("API"),
            "{probs:?}"
        );
    }
    for (alias, label) in [("codex", "codex-cli"), ("agy", "agy-cli")] {
        assert!(unsupported_builtin_tools(&snapshot_with_builtin(alias, &[]), &host).is_empty());
        let probs =
            unsupported_builtin_tools(&snapshot_with_builtin(alias, &[Builtin::Read]), &host);
        assert_eq!(probs.len(), 1, "{alias}: {probs:?}");
        assert!(
            probs[0].contains(label) && probs[0].contains("read"),
            "{probs:?}"
        );
    }
    assert!(
        unsupported_builtin_tools(&snapshot_with_builtin("cli", &[Builtin::Read]), &host)
            .is_empty()
    );
    for bad in [
        &[Builtin::Grep][..],
        &[Builtin::Glob],
        &[Builtin::Read, Builtin::Grep, Builtin::Glob],
    ] {
        let probs = unsupported_builtin_tools(&snapshot_with_builtin("cli", bad), &host);
        assert_eq!(probs.len(), 1, "{bad:?}: {probs:?}");
        assert!(probs[0].contains("claude-cli"), "{probs:?}");
    }
    let gateway_only = host::parse(r#"{"version": 1}"#).unwrap();
    for (alias, h) in [("plain", &host), ("assistant", &gateway_only)] {
        assert!(
            unsupported_builtin_tools(
                &snapshot_with_builtin(alias, &[Builtin::Read, Builtin::Grep, Builtin::Glob]),
                h
            )
            .is_empty(),
            "{alias}"
        );
    }
}
