use super::*;
use crate::host::HostConfig;
use serde_json::json;

fn rejects(value: serde_json::Value) -> String {
    HostConfig::from_value(value).unwrap_err().to_string()
}

fn endpoint() -> serde_json::Value {
    json!({"kind": "openai", "keys": {"automation": {"secret": "automation-api-key", "subject": "build"}}})
}

#[test]
fn reads_an_openai_entrance_next_to_a_webhook_one_and_applies_the_documented_defaults() {
    let host = HostConfig::from_value(json!({"http": {"endpoints": {
        "openai": {"kind": "openai", "wait_ms": 90000, "max_active_runs": 8,
            "keys": {"automation": {"secret": "automation-api-key", "subject": "build"},
                     "automation-next": {"secret": "automation-api-key-2", "subject": "build"}}},
        "inbox": {"issuer": "i", "audience": "a", "key": "inbox-key"},
        "alerts": {"kind": "webhook", "issuer": "i", "audience": "a", "key": "alerts-key"},
        "plain": {"kind": "openai", "keys": {"k": {"secret": "plain-key", "subject": "svc"}}}
    }}}))
    .unwrap();
    assert_eq!(
        host.http.endpoints.keys().collect::<Vec<_>>(),
        ["alerts", "inbox"],
        "omitted kind and webhook kind map to webhook endpoints"
    );
    let openai = &host.http.openai["openai"];
    assert_eq!(openai.keys.len(), 2);
    assert_eq!(openai.keys["automation"].subject, "build");
    assert_eq!(openai.wait(), Duration::from_secs(90));
    assert_eq!(openai.max_active(), 8);
    let plain = &host.http.openai["plain"];
    assert_eq!(plain.wait(), Duration::from_millis(DEFAULT_WAIT_MS));
    assert_eq!(plain.max_active(), DEFAULT_MAX_ACTIVE_RUNS);
    assert_eq!((DEFAULT_WAIT_MS, DEFAULT_MAX_ACTIVE_RUNS), (120_000, 32));
}

#[test]
fn mixing_the_two_kinds_is_refused_before_start_naming_the_field() {
    for field in WEBHOOK_ONLY {
        let mut e = endpoint();
        e[field] = json!("x");
        let message = rejects(json!({"http": {"endpoints": {"openai": e}}}));
        assert!(
            message.contains(&format!("gateway.http.endpoints.openai.{field}"))
                && message.contains("kind=openai"),
            "{field}: {message}"
        );
    }
    for field in OPENAI_ONLY {
        let message = rejects(json!({"http": {"endpoints": {"inbox": {
            "issuer": "i", "audience": "a", "key": "inbox-key", field: 1}}}}));
        assert!(
            message.contains(&format!("gateway.http.endpoints.inbox.{field}"))
                && message.contains("kind=webhook"),
            "{field}: {message}"
        );
    }
    for (e, needle) in [
        (json!({"kind": "grpc"}), "endpoints.x.kind"),
        (json!({"kind": 1}), "endpoints.x.kind"),
        (json!("openai"), "endpoints.x:"),
        (json!({"kind": "openai"}), "keys"),
        (
            json!({"kind": "openai", "keys": {"a": {"secret": "s-key", "subject": "b", "x": 1}}}),
            "unknown field",
        ),
        (
            json!({"kind": "openai", "keys": {"a": {"secret": "s-key"}}}),
            "subject",
        ),
        (
            json!({"kind": "openai", "keys": {}, "extra": 1}),
            "unknown field",
        ),
    ] {
        let message = rejects(json!({"http": {"endpoints": {"x": e.clone()}}}));
        assert!(message.contains(needle), "{e} -> {message}");
    }
}

#[test]
fn slots_secrets_subjects_and_numbers_are_checked_without_echoing_values() {
    let with = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut e = endpoint();
        edit(&mut e);
        rejects(json!({"http": {"endpoints": {"openai": e}}}))
    };
    let slot = |e: &mut serde_json::Value| e["keys"]["automation"].take();
    assert!(with(&|e| e["keys"] = json!({})).contains("openai.keys"));
    assert!(
        with(&|e| e["keys"] = json!({"Bad Slot": {"secret": "s-key", "subject": "b"}}))
            .contains("keys.Bad Slot")
    );
    assert!(
        with(&|e| {
            let _ = slot(e);
            e["keys"]["automation"] = json!({"secret": "Bad Secret", "subject": "b"});
        })
        .contains("keys.automation.secret")
    );
    for subject in ["a/b", "a b", "", "café", &"x".repeat(129), "a:b"] {
        let message = with(&|e| e["keys"]["automation"]["subject"] = json!(subject));
        assert!(
            message.contains("keys.automation.subject"),
            "{subject}: {message}"
        );
        if subject.len() > 3 {
            assert!(!message.contains(subject), "must not echo secret value");
        }
    }
    // Two slots sharing the same secret name would make one key match two slots.
    let message = with(&|e| {
        e["keys"]["second"] = json!({"secret": "automation-api-key", "subject": "other"})
    });
    assert!(
        message.contains("keys.second.secret") && message.contains("automation"),
        "{message}"
    );
    // Two slots pointing at the same subject are allowed for key rotation.
    assert!(
        HostConfig::from_value(
            json!({"http": {"endpoints": {"openai": {"kind": "openai", "keys": {
            "old": {"secret": "key-a", "subject": "build"},
            "new": {"secret": "key-b", "subject": "build"}}}}}})
        )
        .is_ok()
    );
    for (field, bad) in [
        ("wait_ms", json!(0)),
        ("wait_ms", json!(300_001)),
        ("max_active_runs", json!(0)),
        ("max_active_runs", json!(257)),
    ] {
        let message = with(&|e| e[field] = bad.clone());
        assert!(message.contains(field), "{field}={bad}: {message}");
    }
    for ok in [1u64, 300_000] {
        let mut e = endpoint();
        e["wait_ms"] = json!(ok);
        assert!(HostConfig::from_value(json!({"http": {"endpoints": {"openai": e}}})).is_ok());
    }
    let too_many: serde_json::Map<String, serde_json::Value> = (0..=MAX_KEY_SLOTS)
        .map(|n| {
            (
                format!("slot-{n}"),
                json!({"secret": format!("key-{n}"), "subject": "b"}),
            )
        })
        .collect();
    assert!(with(&|e| e["keys"] = json!(too_many.clone())).contains("openai.keys"));
}

#[test]
fn the_v1_alias_belongs_to_the_openai_path() {
    let message = rejects(json!({"http": {"endpoints": {"v1": endpoint()}}}));
    assert!(
        message.contains("endpoints.v1")
            && (message.contains("reserved") || message.contains("/v1")),
        "{message}"
    );
    let message = rejects(json!({"http": {"endpoints": {
        "openai": endpoint(), "v1": {"issuer": "i", "audience": "a", "key": "v1-key"}}}}));
    assert!(
        message.contains("gateway.http.endpoints.v1")
            && (message.contains("/v1") || message.contains("webhook")),
        "{message}"
    );
    // Legacy standalone webhook alias `v1` remains valid when no OpenAI endpoint is configured.
    assert!(
        HostConfig::from_value(json!({"http": {"endpoints": {
            "v1": {"issuer": "i", "audience": "a", "key": "v1-key"}}}}))
        .is_ok()
    );
}

#[test]
fn the_openai_body_limit_defaults_to_256_kib_and_follows_an_explicit_host_value() {
    assert_eq!(HostConfig::default().openai_body_bytes(), 256 * 1024);
    assert_eq!(
        HostConfig::default().limits().body_bytes,
        64 * 1024,
        "webhook default remains 64 KiB"
    );
    let host = HostConfig::from_value(json!({"limits": {"body_bytes": 1000}})).unwrap();
    assert_eq!(host.openai_body_bytes(), 1000);
    assert!(HostConfig::from_value(json!({"limits": {"body_bytes": 1024 * 1024 + 1}})).is_err());
    assert_eq!(
        HostConfig::from_value(json!({"limits": {"body_bytes": 1024 * 1024}}))
            .unwrap()
            .openai_body_bytes(),
        1024 * 1024
    );
}

#[test]
fn the_sdk_test_fragment_from_the_contract_is_read_as_written() {
    let host = HostConfig::from_value(json!({
        "http": {"endpoints": {"openai": {
            "kind": "openai",
            "keys": {
                "build": {"subject": "build", "secret": "build-api-key"},
                "rotated": {"subject": "build", "secret": "rotated-api-key"},
                "guest": {"subject": "guest", "secret": "guest-api-key"}},
            "wait_ms": 500, "max_active_runs": 1}}},
        "limits": {"body_bytes": 4096}
    }))
    .unwrap();
    let openai = &host.http.openai["openai"];
    assert_eq!(openai.keys.len(), 3);
    assert_eq!(openai.wait(), Duration::from_millis(500));
    assert_eq!(openai.max_active(), 1);
    assert_eq!(
        host.openai_body_bytes(),
        4096,
        "explicit limit overrides the OpenAI default"
    );
    assert!(host.http.endpoints.is_empty());
}

#[test]
fn an_openai_alias_must_be_a_lowercase_kebab_name_and_the_error_names_the_field_not_the_value() {
    for bad in [
        "OpenAI",
        "open ai",
        "open_ai",
        "-open",
        "open-",
        "",
        &"a".repeat(64),
    ] {
        let message = rejects(json!({"http": {"endpoints": {bad: endpoint()}}}));
        assert!(
            message.contains("kebab-case") || message.contains("gateway.http.endpoints."),
            "{bad:?}: {message}"
        );
    }
    HostConfig::from_value(json!({"http": {"endpoints": {"open-ai-2": endpoint()}}})).unwrap();
}
