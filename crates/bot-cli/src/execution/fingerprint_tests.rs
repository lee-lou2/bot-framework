//! Golden tests pinning activation fingerprint bytes per provider kind; changing any byte forces re-activation on existing deployments.

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

fn codex() -> Value {
    json!({"kind": "codex-cli", "model": "gpt-x", "endpoint": "https://api.openai.com/v1", "secret": "openai-key"})
}

fn agy() -> Value {
    json!({"kind": "agy-cli", "model": "gemini-x", "endpoint": "https://generativelanguage.googleapis.com", "secret": "gemini-key"})
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
            "codex-cli",
            codex(),
            "8854346e8b95184443cad13cfa9ced6bbb57323c1bf09b5dfe1e5d90c6d173eb",
        ),
        (
            "agy-cli",
            agy(),
            "f6976b7e03e0f06382e88fbf1d269a15c683fe5b1db7814af5ca3887baf7700d",
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
    for base in [claude(), codex(), agy(), anthropic(), openai()] {
        let mut tuned = base.clone();
        tuned["effort"] = json!("medium");
        assert_eq!(print(json!({"a": tuned})), print(json!({"a": base})));
    }
}

#[test]
fn codex_and_agy_cli_kinds_have_distinct_stable_fingerprint_tags_and_apple_images_bind_authority() {
    let same_claude = print(
        json!({"a": json!({"kind": "claude-cli", "model": "m", "endpoint": "https://api.example.com/v1", "secret": "k"})}),
    );
    let same_codex = print(
        json!({"a": json!({"kind": "codex-cli", "model": "m", "endpoint": "https://api.example.com/v1", "secret": "k"})}),
    );
    let same_agy = print(
        json!({"a": json!({"kind": "agy-cli", "model": "m", "endpoint": "https://api.example.com/v1", "secret": "k"})}),
    );
    assert_ne!(same_claude, same_codex);
    assert_ne!(same_claude, same_agy);
    assert_ne!(same_codex, same_agy);

    let digest_a = format!("img@sha256:{}", "a".repeat(64));
    let digest_b = format!("img@sha256:{}", "b".repeat(64));
    let fp = |sandbox: Value| {
        let sandbox = parse_sandbox(&sandbox).unwrap();
        authority_fingerprint(
            &GATEWAY,
            &HostCatalog::default(),
            &BTreeMap::new(),
            &sandbox,
            &BTreeMap::new(),
        )
    };
    let hex = |bytes: [u8; 32]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
    let legacy = fp(json!({"apple": {"image": digest_a, "network": "net"}}));
    let multi = fp(json!({"apple": {"images": {"codex": digest_a}, "network": "net"}}));
    let multi_changed = fp(json!({"apple": {"images": {"codex": digest_b}, "network": "net"}}));
    let multi_both =
        fp(json!({"apple": {"images": {"codex": digest_a, "agy": digest_b}, "network": "net"}}));
    assert_eq!(
        hex(legacy),
        "4638d136a15ae0b212f30bb4c898e02c6b874223a0cfdae85be521ed0ecbfd14"
    );
    assert_eq!(
        hex(multi),
        "3d3945b01c2995d21919505e7b680598647fcc7404792d538bb4bd9aeecdd121"
    );
    assert_ne!(legacy, multi);
    assert_ne!(multi, multi_changed);
    assert_ne!(multi, multi_both);
}
