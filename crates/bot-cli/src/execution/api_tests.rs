use super::*;
use bot_runtime::model::{ApiKind, Effort, LimitField};
use serde_json::json;

fn parsed(entry: Value) -> Result<Provider, String> {
    parse_providers(&json!({"a": entry})).map(|mut all| all.remove("a").unwrap())
}

fn anthropic(patch: Value) -> Value {
    let mut base = json!({"kind": "anthropic-api", "model": "claude-x", "endpoint": "https://api.anthropic.com", "secret": "anthropic-key"});
    base.as_object_mut()
        .unwrap()
        .extend(patch.as_object().unwrap().clone());
    base
}

#[test]
fn both_api_kinds_take_a_model_an_endpoint_a_secret_name_and_an_optional_output_limit() {
    assert_eq!(
        parsed(anthropic(json!({}))).unwrap(),
        Provider::Api(ApiProvider {
            kind: ApiKind::Anthropic,
            model: "claude-x".into(),
            endpoint: "https://api.anthropic.com".into(),
            secret: Some("anthropic-key".into()),
            auth: None,
            max_tokens: None,
            limit_field: LimitField::MaxTokens,
            effort: None,
            fallback: None,
        })
    );
    assert_eq!(
        parsed(json!({"kind": "openai-api", "model": "gpt-x", "endpoint": "https://api.example.com/v1", "secret": "openai-key", "max_tokens": 2048})).unwrap(),
        Provider::Api(ApiProvider {
            kind: ApiKind::OpenAi,
            model: "gpt-x".into(),
            endpoint: "https://api.example.com/v1".into(),
            secret: Some("openai-key".into()),
            auth: None,
            max_tokens: Some(2048),
            limit_field: LimitField::MaxTokens,
            effort: None,
            fallback: None,
        })
    );
    for endpoint in [
        "http://127.0.0.1:11434/v1",
        "http://[::1]:8000/v1",
        "https://127.0.0.1:9443/v1",
    ] {
        let local =
            parsed(json!({"kind": "openai-api", "model": "llama", "endpoint": endpoint})).unwrap();
        assert_eq!(
            local,
            Provider::Api(ApiProvider {
                kind: ApiKind::OpenAi,
                model: "llama".into(),
                endpoint: endpoint.into(),
                secret: None,
                auth: None,
                max_tokens: None,
                limit_field: LimitField::MaxTokens,
                effort: None,
                fallback: None,
            })
        );
    }
    let all = parse_providers(&json!({
        "a": {"kind": "demo"},
        "b": {"kind": "anthropic-api", "model": "m", "endpoint": "https://h.example", "secret": "k"},
        "c": {"kind": "claude-cli", "model": "m", "endpoint": "https://h.example/v1", "secret": "k"},
    }))
    .unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn unsafe_destinations_missing_keys_and_odd_fields_are_refused_with_the_field_path() {
    let openai = |patch: Value| {
        let mut base = json!({"kind": "openai-api", "model": "m", "endpoint": "https://h.example/v1", "secret": "k"});
        base.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        base
    };
    for (entry, needle) in [
        (
            openai(json!({"endpoint": "http://h.example/v1"})),
            "loopback",
        ),
        (
            openai(json!({"endpoint": "http://localhost:11434/v1"})),
            "loopback",
        ),
        (
            openai(json!({"endpoint": "http://10.0.0.5/v1"})),
            "loopback",
        ),
        (
            openai(json!({"endpoint": "http://[::ffff:127.0.0.1]/v1"})),
            "loopback",
        ),
        (
            openai(json!({"endpoint": "https://user:pw@h.example/v1"})),
            "credentials",
        ),
        (
            openai(json!({"endpoint": "https://h.example/v1?key=1"})),
            "queries",
        ),
        (
            openai(json!({"endpoint": "https://h.example/v1#frag"})),
            "fragments",
        ),
        (
            openai(json!({"endpoint": "https://h.example:0/v1"})),
            "port",
        ),
        (
            openai(json!({"endpoint": "https://h.example/../v1"})),
            "path",
        ),
        (openai(json!({"endpoint": "ftp://h.example"})), "https://"),
        (
            json!({"kind": "openai-api", "model": "m", "secret": "k"}),
            "endpoint",
        ),
        (
            json!({"kind": "openai-api", "model": "m", "endpoint": "https://h.example/v1"}),
            "secret",
        ),
        (openai(json!({"secret": "Bad Name"})), "providers.a.secret"),
        (openai(json!({"secret": ""})), "secret"),
        (openai(json!({"secret": null})), "secret"),
        (openai(json!({"model": ""})), "model"),
        (
            json!({"kind": "openai-api", "endpoint": "https://h.example/v1", "secret": "k"}),
            "model",
        ),
        (openai(json!({"max_tokens": 0})), "max_tokens"),
        (openai(json!({"max_tokens": 1_000_001})), "max_tokens"),
        (openai(json!({"max_tokens": -5})), "max_tokens"),
        (openai(json!({"max_tokens": "4096"})), "max_tokens"),
        (openai(json!({"max_tokens": 1.5})), "max_tokens"),
        (openai(json!({"temperature": 0})), "unknown"),
    ] {
        let error = parsed(entry.clone()).expect_err(&entry.to_string());
        assert!(error.contains("providers.a"), "{entry} -> {error}");
        assert!(error.contains(needle), "{entry} -> {error}");
        // Errors must never echo credential-bearing input values.
        assert!(
            !error.contains("pw@") && !error.contains("user:"),
            "{error}"
        );
    }
    let error = parsed(json!({"kind": "gemini-api"})).unwrap_err();
    assert!(
        error.contains("anthropic-api") && error.contains("openai-api"),
        "{error}"
    );
}

fn providers(provider: Provider) -> BTreeMap<String, Provider> {
    [("assistant".to_owned(), provider)].into()
}

fn fingerprint(providers: &BTreeMap<String, Provider>) -> [u8; 32] {
    let catalog = HostCatalog {
        models: [("personal".to_owned(), ["assistant".to_owned()].into())].into(),
        ..HostCatalog::default()
    };
    authority_fingerprint(
        &[7u8; 32],
        &catalog,
        providers,
        &Sandbox::default(),
        &BTreeMap::new(),
    )
}

fn api(
    kind: u8,
    model: &str,
    endpoint: &str,
    secret: Option<&str>,
    max_tokens: Option<u32>,
) -> Provider {
    let (model, endpoint, secret) = (
        model.to_owned(),
        endpoint.to_owned(),
        secret.map(str::to_owned),
    );
    Provider::Api(ApiProvider {
        kind: if kind == 0 {
            ApiKind::Anthropic
        } else {
            ApiKind::OpenAi
        },
        model,
        endpoint,
        secret,
        auth: None,
        max_tokens,
        limit_field: LimitField::MaxTokens,
        effort: None,
        fallback: None,
    })
}

#[test]
fn the_activation_fingerprint_binds_kind_model_whole_base_url_and_credential_name_but_not_the_limit()
 {
    let base = api(0, "m", "https://h.example/v1", Some("key"), None);
    let print = |provider: Provider| fingerprint(&providers(provider));
    assert_eq!(print(base.clone()), print(base.clone()), "deterministic");
    for (what, changed) in [
        (
            "kind",
            api(1, "m", "https://h.example/v1", Some("key"), None),
        ),
        (
            "model",
            api(0, "m2", "https://h.example/v1", Some("key"), None),
        ),
        (
            "host",
            api(0, "m", "https://other.example/v1", Some("key"), None),
        ),
        (
            "path",
            api(0, "m", "https://h.example/v2", Some("key"), None),
        ),
        (
            "port",
            api(0, "m", "https://h.example:8443/v1", Some("key"), None),
        ),
        (
            "secret name",
            api(0, "m", "https://h.example/v1", Some("key2"), None),
        ),
        ("no secret", api(0, "m", "https://h.example/v1", None, None)),
    ] {
        assert_ne!(print(base.clone()), print(changed), "{what}");
    }
    assert_eq!(
        print(base.clone()),
        print(api(0, "m", "https://h.example/v1", Some("key"), Some(8192)))
    );
    let cli = Provider::Cli(CliProvider {
        kind: CliKind::Claude,
        model: "m".into(),
        endpoint: "https://h.example/v1".into(),
        secret: Some("key".into()),
        auth: None,
        max_tokens: None,
        effort: None,
        fallback: None,
    });
    assert_ne!(print(base.clone()), print(cli));
    assert_ne!(print(base), print(Provider::Demo));
    // Absent secret vs. empty name must remain distinct via tag byte.
    assert_ne!(
        print(api(1, "m", "http://127.0.0.1:1/v1", None, None)),
        print(api(1, "m", "http://127.0.0.1:1/v1", Some(""), None))
    );
}

#[test]
fn openai_api_takes_max_completion_tokens_or_max_tokens_but_never_both() {
    let openai = |patch: Value| {
        let mut base = json!({"kind": "openai-api", "model": "o3", "endpoint": "https://api.openai.com/v1", "secret": "openai-key"});
        base.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        base
    };
    let want = |max_tokens, limit_field| {
        Provider::Api(ApiProvider {
            kind: ApiKind::OpenAi,
            model: "o3".into(),
            endpoint: "https://api.openai.com/v1".into(),
            secret: Some("openai-key".into()),
            auth: None,
            max_tokens,
            limit_field,
            effort: None,
            fallback: None,
        })
    };
    assert_eq!(
        parsed(openai(json!({"max_completion_tokens": 2048}))).unwrap(),
        want(Some(2048), LimitField::MaxCompletionTokens)
    );
    assert_eq!(
        parsed(openai(json!({"max_tokens": 2048}))).unwrap(),
        want(Some(2048), LimitField::MaxTokens)
    );
    assert_eq!(
        parsed(openai(json!({}))).unwrap(),
        want(None, LimitField::MaxTokens)
    );
    for bad in [
        json!({"max_tokens": 1, "max_completion_tokens": 1}),
        json!({"max_completion_tokens": 0}),
        json!({"max_completion_tokens": 1_000_001}),
        json!({"max_completion_tokens": "x"}),
    ] {
        assert!(parsed(openai(bad.clone())).is_err(), "{bad}");
    }
    assert!(parsed(anthropic(json!({"max_completion_tokens": 100}))).is_err());
}

#[test]
fn effort_accepts_five_levels_on_non_demo_providers_and_refuses_bad_values() {
    for (raw, expected) in [
        ("low", Effort::Low),
        ("medium", Effort::Medium),
        ("high", Effort::High),
        ("xhigh", Effort::Xhigh),
        ("max", Effort::Max),
    ] {
        assert_eq!(
            parsed(anthropic(json!({"effort": raw}))).unwrap(),
            Provider::Api(ApiProvider {
                kind: ApiKind::Anthropic,
                model: "claude-x".into(),
                endpoint: "https://api.anthropic.com".into(),
                secret: Some("anthropic-key".into()),
                auth: None,
                max_tokens: None,
                limit_field: LimitField::MaxTokens,
                effort: Some(expected),
                fallback: None,
            })
        );
        assert_eq!(
            parsed(json!({
                "kind": "openai-api",
                "model": "gpt-x",
                "endpoint": "https://api.openai.com/v1",
                "secret": "openai-key",
                "effort": raw
            }))
            .unwrap(),
            Provider::Api(ApiProvider {
                kind: ApiKind::OpenAi,
                model: "gpt-x".into(),
                endpoint: "https://api.openai.com/v1".into(),
                secret: Some("openai-key".into()),
                auth: None,
                max_tokens: None,
                limit_field: LimitField::MaxTokens,
                effort: Some(expected),
                fallback: None,
            })
        );
        assert_eq!(
            parsed(json!({
                "kind": "claude-cli",
                "model": "claude-x",
                "endpoint": "https://api.example.com/v1",
                "secret": "claude-key",
                "effort": raw
            }))
            .unwrap(),
            Provider::Cli(CliProvider {
                kind: CliKind::Claude,
                model: "claude-x".into(),
                endpoint: "https://api.example.com/v1".into(),
                secret: Some("claude-key".into()),
                auth: None,
                max_tokens: None,
                effort: Some(expected),
                fallback: None,
            })
        );
    }
    for bad in [
        json!("minimal"),
        json!("MEDIUM"),
        json!(""),
        json!(1),
        json!(true),
        json!(null),
    ] {
        let error = parsed(anthropic(json!({"effort": bad.clone()}))).unwrap_err();
        assert!(error.contains("providers.a.effort"), "{bad} -> {error}");
    }
    let error = parsed(json!({"kind": "demo", "effort": "medium"})).unwrap_err();
    assert!(error.contains("unknown"), "{error}");
}
