//! 제공자 종류별 사실(`impl Provider`): 서비스 키 자격 이름·모델 VM 필요 여부·운영자용 한 줄 설명. 새 종류를 더하면 이 표에도 한 줄을 더한다.

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
        // 키 없이 쓸 수 있는 것은 loopback 로컬 서버뿐이다
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
        (json!({"kind": "demo"}), "a: demo(외부 호출 없는 데모 모델)"),
        (
            claude(),
            "a: claude-cli · 모델 m · 상류 h.example · 자격 이름 claude-key",
        ),
        (
            json!({"kind": "anthropic-api", "model": "m", "endpoint": "https://h.example/v1", "secret": "a-key", "max_tokens": 100}),
            "a: anthropic-api · 모델 m · 상류 h.example · 자격 이름 a-key · 최대 토큰 100",
        ),
        (
            json!({"kind": "openai-api", "model": "m", "endpoint": "https://h.example:8443/v1", "secret": "o-key", "max_completion_tokens": 7}),
            "a: openai-api · 모델 m · 상류 h.example:8443 · 자격 이름 o-key · 최대 토큰 7(max_completion_tokens)",
        ),
        (
            json!({"kind": "openai-api", "model": "m", "endpoint": "http://127.0.0.1:11434/v1"}),
            "a: openai-api · 모델 m · 상류 127.0.0.1:11434 · 서비스 키 없음(loopback 전용)",
        ),
    ] {
        assert_eq!(provider(entry).describe("a"), expected);
    }
}
