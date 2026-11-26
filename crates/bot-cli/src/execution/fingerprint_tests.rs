//! 활성화 지문의 고정점: 호스트 설정(`providers` 절의 JSON)이 같으면 제공자 종류마다 지문 바이트가 그대로다.
//! 바이트 하나가 달라지면 이미 활성화된 모든 배포가 재활성화를 요구받으므로, 지문 재료를 다루는 코드를 고치기 전에 이 값을 먼저 고정한다.
//! 입력은 호스트 설정의 JSON이라 `Provider`의 내부 모양이 바뀌어도 이 시험은 그대로다.

use super::*;
use serde_json::json;

const GATEWAY: [u8; 32] = [7u8; 32];

fn print(providers: Value) -> String {
    let providers = parse_providers(&providers).unwrap();
    let catalog = HostCatalog {
        models: [("personal".to_owned(), providers.keys().cloned().collect())].into(),
        ..HostCatalog::default()
    };
    authority_fingerprint(
        &GATEWAY,
        &catalog,
        &providers,
        &Sandbox::default(),
        &BTreeMap::new(),
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

fn demo() -> Value {
    json!({"kind": "demo"})
}

fn claude() -> Value {
    json!({"kind": "claude-cli", "model": "claude-x", "endpoint": "https://api.example.com/v1", "secret": "claude-key"})
}

fn anthropic() -> Value {
    json!({"kind": "anthropic-api", "model": "claude-x", "endpoint": "https://api.anthropic.com", "secret": "anthropic-key"})
}

fn anthropic_keyless() -> Value {
    json!({"kind": "anthropic-api", "model": "local", "endpoint": "http://127.0.0.1:8080"})
}

fn openai() -> Value {
    json!({"kind": "openai-api", "model": "gpt-x", "endpoint": "https://api.openai.com/v1", "secret": "openai-key"})
}

fn openai_keyless() -> Value {
    json!({"kind": "openai-api", "model": "llama", "endpoint": "http://127.0.0.1:11434/v1"})
}

#[test]
fn every_provider_kind_keeps_its_fingerprint_bytes() {
    for (name, entry, expected) in [
        (
            "demo",
            demo(),
            "6cda843894ab23afa4f8a0a48225319ee943cc0942b7b569be13ea733ed49ce3",
        ),
        (
            "claude-cli",
            claude(),
            "a334ba03ed95ccf087a2a4166fee34595c971b6907df06eeaef0d2d2eda0b0f0",
        ),
        (
            "anthropic-api",
            anthropic(),
            "707470e02fa8a44bcefea4aff44bea8a091f21e3a4ecff580735aa5c7fee39b1",
        ),
        (
            "anthropic-api, no key",
            anthropic_keyless(),
            "2af7cc53b61e4e80ffd5bf9a9734f39322ba33649ca2fc975f67402ee312a603",
        ),
        (
            "openai-api",
            openai(),
            "12146ac747089b07eedd335d87552ac1f6c01a9fe275e45a2d108faa0e85b93d",
        ),
        (
            "openai-api, no key",
            openai_keyless(),
            "4cdb7bb835b047f9354e478e8cd59a285fb9cdad0cbc41e7b05f6936ddad932e",
        ),
    ] {
        assert_eq!(print(json!({"a": entry})), expected, "{name}");
    }
}

#[test]
fn a_host_with_every_kind_keeps_its_fingerprint_bytes() {
    let all = json!({"a": demo(), "b": claude(), "c": anthropic(), "d": anthropic_keyless(), "e": openai(), "f": openai_keyless()});
    assert_eq!(
        print(all),
        "f6f536f8615663a10eadf2f0ff2ff358a2ed95420feff5ef2430ec4c61e6972c"
    );
}

#[test]
fn output_limits_are_not_authority_so_they_leave_the_bytes_alone() {
    let mut limited = anthropic();
    limited["max_tokens"] = json!(8192);
    assert_eq!(
        print(json!({"a": limited})),
        print(json!({"a": anthropic()}))
    );
    let mut limited = openai();
    limited["max_completion_tokens"] = json!(2048);
    assert_eq!(print(json!({"a": limited})), print(json!({"a": openai()})));
    let mut limited = openai();
    limited["max_tokens"] = json!(2048);
    assert_eq!(print(json!({"a": limited})), print(json!({"a": openai()})));
}

#[test]
fn effort_is_not_authority_so_it_leaves_the_bytes_alone() {
    for base in [claude(), anthropic(), openai()] {
        let mut tuned = base.clone();
        tuned["effort"] = json!("medium");
        assert_eq!(print(json!({"a": tuned})), print(json!({"a": base})));
    }
}
