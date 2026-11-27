use super::*;
use serde_json::json;

fn provider(entry: Value) -> Provider {
    parse_providers(&json!({"a": entry}))
        .unwrap()
        .remove("a")
        .unwrap()
}

fn claude() -> Value {
    json!({"kind": "claude-cli", "model": "m", "endpoint": "https://h.example/v1/private-path", "secret": "claude-key"})
}

#[test]
fn each_kind_names_the_credential_it_needs_and_whether_it_runs_in_the_model_vm() {
    for (entry, secret, vm) in [
        (json!({"kind": "demo"}), None, false),
        (claude(), Some("claude-key"), true),
        (
            json!({"kind": "anthropic-api", "model": "m", "endpoint": "https://h.example", "secret": "a-key"}),
            Some("a-key"),
            false,
        ),
        (
            json!({"kind": "openai-api", "model": "m", "endpoint": "https://h.example/v1", "secret": "o-key"}),
            Some("o-key"),
            false,
        ),
        (
            json!({"kind": "openai-api", "model": "m", "endpoint": "http://127.0.0.1:11434/v1"}),
            None,
            false,
        ),
    ] {
        let provider = provider(entry.clone());
        assert_eq!(provider.secret(), secret, "{entry}");
        assert_eq!(provider.needs_model_vm(), vm, "{entry}");
    }
}

#[test]
fn the_description_shows_names_and_hosts_but_neither_paths_nor_values() {
    for (entry, expected) in [
        (
            json!({"kind": "demo"}),
            "a: demo (local demo model, no external calls)",
        ),
        (
            claude(),
            "a: claude-cli · model m · upstream h.example · secret claude-key",
        ),
        (
            json!({"kind": "claude-cli", "model": "m", "endpoint": "https://h.example/v1/private-path", "secret": "claude-key", "effort": "high"}),
            "a: claude-cli · model m · upstream h.example · secret claude-key · effort high",
        ),
        (
            json!({"kind": "anthropic-api", "model": "m", "endpoint": "https://h.example/v1", "secret": "a-key", "max_tokens": 100, "effort": "medium"}),
            "a: anthropic-api · model m · upstream h.example · secret a-key · max tokens 100 · effort medium",
        ),
        (
            json!({"kind": "openai-api", "model": "m", "endpoint": "https://h.example:8443/v1", "secret": "o-key", "max_completion_tokens": 7, "effort": "low"}),
            "a: openai-api · model m · upstream h.example:8443 · secret o-key · max tokens 7 (max_completion_tokens) · effort low",
        ),
        (
            json!({"kind": "openai-api", "model": "m", "endpoint": "http://127.0.0.1:11434/v1"}),
            "a: openai-api · model m · upstream 127.0.0.1:11434 · no service key (loopback only)",
        ),
    ] {
        assert_eq!(provider(entry).describe("a"), expected);
    }
}
