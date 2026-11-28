use super::*;
use crate::testing::{FakeSecrets, FakeUpstream, ManualClock, MemLedger, run_context};

const KEY: &str = "provider-key-for-personal";

struct Rig {
    proxy: ModelProxy,
    ledger: Arc<MemLedger>,
    upstream: Arc<FakeUpstream>,
}

fn provider(name: &str, host: &str, secret: &str, aliases: &[(&str, &str)]) -> (String, Provider) {
    (
        name.to_owned(),
        Provider {
            protocol: Protocol::Anthropic,
            endpoint: HostPort {
                host: host.into(),
                port: 443,
            },
            path: "/v1/messages".into(),
            auth: AuthSource::Secret(SecretName::parse(secret).unwrap()),
            header_mode: HeaderMode::Bearer,
            headers: vec![("x-api-version".into(), "1".into())],
            forward_headers: vec!["anthropic-version".into(), "anthropic-beta".into()],
            // Only "other" allows an extra operator-configured top-level body field.
            body_fields: if name == "other" {
                vec!["x_extra".into()]
            } else {
                vec![]
            },
            models: aliases
                .iter()
                .map(|(a, m)| (a.to_string(), m.to_string()))
                .collect(),
        },
    )
}

fn rig(limits: impl FnOnce(&mut ModelLimits)) -> Rig {
    let host = ModelHost {
        providers: [
            provider(
                "main",
                "api.main.example",
                "model-key",
                &[("assistant", "model-large"), ("fast", "model-small")],
            ),
            provider(
                "other",
                "api.other.example",
                "model-key",
                &[("assistant", "other-large"), ("solo", "other-solo")],
            ),
            provider(
                "secret",
                "api.hidden.example",
                "model-key",
                &[("hidden", "hidden-1")],
            ),
        ]
        .into(),
        allow: [
            (ProfileId("personal".into()), ["main".to_owned()].into()),
            (
                ProfileId("both".into()),
                ["main".to_owned(), "other".to_owned()].into(),
            ),
            (ProfileId("solo".into()), ["other".to_owned()].into()),
        ]
        .into(),
    };
    let clock = Arc::new(ManualClock::new(1_000));
    let ledger = Arc::new(MemLedger::new(clock.clone()));
    for profile in ["personal", "both", "solo", "stranger"] {
        ledger.add_run(run_context(
            profile,
            &format!("run-{profile}"),
            1,
            "local:owner",
            "main",
            &["run"],
        ));
    }
    let secrets = Arc::new(FakeSecrets::new(&[
        ("personal", "model-key", KEY),
        ("both", "model-key", "both-key-value"),
        ("solo", "model-key", "solo-key-value"),
    ]));
    let upstream = Arc::new(FakeUpstream::new());
    let mut cfg = ModelLimits::default();
    limits(&mut cfg);
    let proxy = ModelProxy::new(host, ledger.clone(), secrets, upstream.clone(), clock, cfg);
    Rig {
        proxy,
        ledger,
        upstream,
    }
}

fn request(profile: &str, alias: &str, body: &str) -> ModelRequest {
    ModelRequest {
        attempt: Attempt {
            profile: ProfileId(profile.into()),
            run: RunId(format!("run-{profile}")),
            generation: Generation(1),
            claim: Some(ClaimEpoch(1)),
        },
        alias: alias.into(),
        body: JsonPayload(body.into()),
        headers: Vec::new(),
    }
}

async fn ask(r: &Rig, profile: &str, alias: &str, body: &str) -> Result<ModelResponse, Error> {
    r.proxy.forward(request(profile, alias, body)).await
}

#[tokio::test]
async fn the_key_goes_upstream_only_and_the_model_name_is_the_operators() {
    let r = rig(|_| {});
    let reply = ask(
        &r,
        "personal",
        "assistant",
        r#"{"messages":[{"role":"user","content":"hi"}],"model":"attacker-model"}"#,
    )
    .await
    .unwrap();
    assert_eq!(
        (reply.status, reply.body.0.as_str()),
        (200, "{\"ok\":true}")
    );
    let sent = r.upstream.requests();
    assert_eq!(sent.len(), 1);
    let (endpoint, path, headers, body) = &sent[0];
    assert_eq!(
        (endpoint.as_str(), path.as_str()),
        ("api.main.example:443", "/v1/messages")
    );
    assert!(headers.contains(&("authorization".into(), format!("Bearer {KEY}"))));
    assert!(headers.contains(&("x-api-version".into(), "1".into())));
    assert_eq!(
        body, r#"{"messages":[{"content":"hi","role":"user"}],"model":"model-large"}"#,
        "sandbox-supplied model must be overwritten"
    );
    assert!(
        !reply.body.0.contains(KEY) && !body.contains(KEY),
        "provider key must not appear in body or response"
    );
}

#[tokio::test]
async fn providers_outside_the_profile_allow_list_are_never_contacted() {
    let r = rig(|_| {});
    for (profile, alias) in [
        ("personal", "hidden"),
        ("personal", "solo"),
        ("stranger", "assistant"),
        ("personal", "nope"),
    ] {
        assert!(
            matches!(ask(&r, profile, alias, "{}").await, Err(Error::Denied)),
            "{profile}/{alias}"
        );
    }
    assert!(
        r.upstream.requests().is_empty(),
        "disallowed providers must never be contacted"
    );
    // Ambiguous alias claimed by two allowed providers is denied.
    assert!(matches!(
        ask(&r, "both", "assistant", "{}").await,
        Err(Error::Denied)
    ));
    assert!(ask(&r, "solo", "solo", "{}").await.is_ok());
    assert_eq!(r.upstream.requests()[0].0, "api.other.example:443");
}

#[tokio::test]
async fn each_profile_uses_its_own_key_namespace() {
    let r = rig(|_| {});
    ask(&r, "personal", "assistant", "{}").await.unwrap();
    ask(&r, "solo", "solo", "{}").await.unwrap();
    let sent = r.upstream.requests();
    assert!(
        sent[0]
            .2
            .contains(&("authorization".into(), format!("Bearer {KEY}")))
    );
    assert!(
        sent[1]
            .2
            .contains(&("authorization".into(), "Bearer solo-key-value".into()))
    );
    r.ledger.add_run(run_context(
        "stranger",
        "run-stranger",
        1,
        "local:owner",
        "main",
        &["run"],
    ));
    assert!(matches!(
        ask(&r, "stranger", "assistant", "{}").await,
        Err(Error::Denied)
    ));
}

#[tokio::test]
async fn a_missing_key_sends_nothing() {
    let r = rig(|_| {});
    r.ledger.add_run(run_context(
        "both",
        "run-both",
        1,
        "local:owner",
        "main",
        &["run"],
    ));
    assert!(ask(&r, "both", "fast", "{}").await.is_ok());
    let mut host = ModelHost::default();
    host.providers.extend([provider(
        "main",
        "api.main.example",
        "model-key",
        &[("assistant", "m")],
    )]);
    host.allow
        .insert(ProfileId("personal".into()), ["main".to_owned()].into());
    let clock = Arc::new(ManualClock::new(1_000));
    let ledger = Arc::new(MemLedger::new(clock.clone()));
    ledger.add_run(run_context(
        "personal",
        "run-personal",
        1,
        "local:owner",
        "main",
        &["run"],
    ));
    let upstream = Arc::new(FakeUpstream::new());
    let proxy = ModelProxy::new(
        host,
        ledger,
        Arc::new(FakeSecrets::new(&[])),
        upstream.clone(),
        clock,
        ModelLimits::default(),
    );
    assert!(matches!(
        proxy.forward(request("personal", "assistant", "{}")).await,
        Err(Error::Unavailable)
    ));
    assert!(upstream.requests().is_empty());
}

#[tokio::test]
async fn a_message_from_an_old_owner_never_reaches_the_provider() {
    let r = rig(|_| {});
    let mut anonymous = request("personal", "assistant", "{}");
    anonymous.attempt.claim = None;
    assert!(matches!(
        r.proxy.forward(anonymous).await,
        Err(Error::Denied)
    ));
    r.ledger.bump_claim("run-personal");
    assert!(
        matches!(
            ask(&r, "personal", "assistant", "{}").await,
            Err(Error::Stale)
        ),
        "stale claim"
    );
    let mut taken = request("personal", "assistant", "{}");
    taken.attempt.claim = Some(r.ledger.claim_epoch("run-personal"));
    r.ledger.bump_generation("run-personal");
    assert!(
        matches!(r.proxy.forward(taken).await, Err(Error::Stale)),
        "stale generation"
    );
    r.ledger.cancel("run-personal");
    assert!(
        r.upstream.requests().is_empty(),
        "old owner's request must not reach upstream"
    );
}

#[tokio::test]
async fn request_bodies_must_be_clean_objects() {
    let r = rig(|c| c.max_request = 64);
    for bad in [
        "not json",
        "[1]",
        r#"{"a":1,"a":2}"#,
        r#"{"stream":true}"#,
        r#"{"n":9007199254740993}"#,
        &format!("{{\"x\":\"{}\"}}", "a".repeat(80)),
    ] {
        assert!(
            matches!(
                ask(&r, "personal", "assistant", bad).await,
                Err(Error::Invalid)
            ),
            "{bad}"
        );
    }
    assert!(
        ask(&r, "personal", "assistant", r#"{"stream":false}"#)
            .await
            .is_ok()
    );
    assert_eq!(r.upstream.requests().len(), 1);
}

#[tokio::test]
async fn a_response_that_echoes_the_key_is_withheld() {
    let r = rig(|_| {});
    r.upstream
        .respond(200, format!("{{\"echo\":\"{KEY}\"}}").as_bytes());
    assert!(matches!(
        ask(&r, "personal", "assistant", "{}").await,
        Err(Error::Denied)
    ));
    // Unicode-escaped credentials are caught after decoding.
    let escaped = KEY.replacen('p', "\\u0070", 1);
    r.upstream
        .respond(200, format!("{{\"echo\":\"{escaped}\"}}").as_bytes());
    assert!(matches!(
        ask(&r, "personal", "assistant", "{}").await,
        Err(Error::Denied)
    ));
    r.upstream
        .respond(200, format!("{{\"{escaped}\":1}}").as_bytes());
    assert!(
        matches!(
            ask(&r, "personal", "assistant", "{}").await,
            Err(Error::Denied)
        ),
        "credential hidden in object key must be blocked"
    );
    r.upstream
        .respond(429, b"{\"error\":\"account acct-123 is over quota\"}");
    let reply = ask(&r, "personal", "assistant", "{}").await.unwrap();
    assert_eq!(
        (reply.status, reply.body.0.as_str()),
        (429, r#"{"error":{"status":429}}"#)
    );
    r.upstream.respond(200, b"not json");
    assert!(matches!(
        ask(&r, "personal", "assistant", "{}").await,
        Err(Error::Unavailable)
    ));
    r.upstream
        .respond(200, &vec![b'a'; ModelLimits::default().max_response + 1]);
    assert!(matches!(
        ask(&r, "personal", "assistant", "{}").await,
        Err(Error::Unavailable)
    ));
    r.upstream.fail();
    assert!(matches!(
        ask(&r, "personal", "assistant", "{}").await,
        Err(Error::Unavailable)
    ));
}

#[tokio::test]
async fn model_calls_are_rate_limited_per_principal() {
    let r = rig(|c| {
        c.rate = RateConfig {
            per_minute: 60,
            burst: 2,
            concurrent: 4,
        }
    });
    ask(&r, "personal", "assistant", "{}").await.unwrap();
    ask(&r, "personal", "assistant", "{}").await.unwrap();
    assert!(matches!(
        ask(&r, "personal", "assistant", "{}").await,
        Err(Error::Unavailable)
    ));
    assert_eq!(r.upstream.requests().len(), 2);
}

#[tokio::test]
async fn provider_side_tools_and_remote_connections_never_reach_the_upstream() {
    let r = rig(|_| {});
    let tool = r#"{"name":"skill_tool","description":"d","input_schema":{"type":"object"}}"#;
    let bad = [
        // Provider-hosted tools (web search, code execution, bash, editor).
        r#"{"tools":[{"type":"web_search_20250305","name":"web_search"}]}"#,
        r#"{"tools":[{"type":"code_execution_20250522","name":"code_execution"}]}"#,
        r#"{"tools":[{"type":"bash_20250124","name":"bash"}]}"#,
        r#"{"tools":[{"type":"text_editor_20250124","name":"str_replace_editor"}]}"#,
        // Malformed custom tool declarations.
        r#"{"tools":[{"type":"custom","name":"x"}]}"#,
        r#"{"tools":[{"name":"x","input_schema":{},"max_uses":5}]}"#,
        r#"{"tools":[{"name":"bad name","input_schema":{}}]}"#,
        r#"{"tools":["web_search"]}"#,
        r#"{"tools":{"name":"x"}}"#,
        // Provider-side remote connectors or unknown top-level features.
        r#"{"mcp_servers":[{"type":"url","url":"http://attacker.example/mcp","name":"s"}]}"#,
        r#"{"container":"container_abc"}"#,
        r#"{"service_tier":"auto"}"#,
        r#"{"x_extra":1}"#,
        // Invalid tool_choice shapes.
        r#"{"tool_choice":"auto"}"#,
        r#"{"tool_choice":{"type":"web_search"}}"#,
        r#"{"tool_choice":{"type":"tool"}}"#,
        r#"{"tool_choice":{"type":"auto","name":"x"}}"#,
    ];
    for body in bad {
        assert!(
            matches!(
                ask(&r, "personal", "assistant", body).await,
                Err(Error::Invalid)
            ),
            "must reject: {body}"
        );
    }
    let mixed =
        format!(r#"{{"tools":[{tool},{{"type":"web_search_20250305","name":"web_search"}}]}}"#);
    assert!(matches!(
        ask(&r, "personal", "assistant", &mixed).await,
        Err(Error::Invalid)
    ));
    assert!(
        r.upstream.requests().is_empty(),
        "rejected requests must not reach upstream"
    );
}

#[tokio::test]
async fn registered_client_tools_and_ordinary_fields_pass() {
    let r = rig(|_| {});
    let body = r#"{"max_tokens":64,"system":"s","metadata":{"user_id":"u"},"thinking":{"type":"enabled","budget_tokens":16},"temperature":0.2,"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"skill_tool","description":"d","input_schema":{"type":"object","properties":{}},"cache_control":{"type":"ephemeral"}},{"type":"custom","name":"other-tool","input_schema":{}}],"tool_choice":{"type":"auto","disable_parallel_tool_use":true}}"#;
    ask(&r, "personal", "assistant", body).await.unwrap();
    let sent = r.upstream.requests();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].3.contains("\"skill_tool\"") && sent[0].3.contains("\"model-large\""));
    let named = r#"{"tools":[{"name":"skill_tool","input_schema":{}}],"tool_choice":{"type":"tool","name":"skill_tool"}}"#;
    ask(&r, "personal", "assistant", named).await.unwrap();
    let none = r#"{"tools":[],"tool_choice":{"type":"none"}}"#;
    ask(&r, "personal", "assistant", none).await.unwrap();
}

#[tokio::test]
async fn an_extra_body_field_is_allowed_only_for_the_provider_that_lists_it() {
    let r = rig(|_| {});
    let body = r#"{"messages":[],"x_extra":1}"#;
    assert!(matches!(
        ask(&r, "personal", "assistant", body).await,
        Err(Error::Invalid)
    ));
    ask(&r, "solo", "solo", body).await.unwrap();
    let tool = r#"{"x_extra":1,"tools":[{"type":"web_search_20250305","name":"web_search"}]}"#;
    assert!(matches!(
        ask(&r, "solo", "solo", tool).await,
        Err(Error::Invalid)
    ));
}

#[tokio::test]
async fn a_remote_attachment_never_reaches_the_upstream_but_an_inline_one_does() {
    let r = rig(|_| {});
    let remote = [
        r#"{"messages":[{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://attacker.example/p.png?d=SECRET"}}]}]}"#,
        r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":[{"type":"image","source":{"type":"file","file_id":"file_1"}}]}]}]}"#,
        r#"{"messages":[{"role":"user","content":[{"type":"document","source":{"type":"url","url":"https://attacker.example/d.pdf"}}]}]}"#,
    ];
    for body in remote {
        assert!(
            matches!(
                ask(&r, "personal", "assistant", body).await,
                Err(Error::Invalid)
            ),
            "{body}"
        );
    }
    assert!(r.upstream.requests().is_empty());
    let inline = r#"{"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}]}"#;
    ask(&r, "personal", "assistant", inline).await.unwrap();
    assert_eq!(r.upstream.requests().len(), 1);
}

#[tokio::test]
async fn bearer_merge_header_merges_forwarded_anthropic_beta_and_attaches_extra_headers() {
    use bot_core::{
        auth::{AuthReply, CredentialKind, ResolvedAuth},
        port::SecretValue,
    };
    struct CustomAuth;
    impl AuthResolver for CustomAuth {
        fn resolve(
            &self,
            _profile: ProfileId,
            source: AuthSource,
            surface: CallerSurface,
            _force_refresh: bool,
        ) -> AuthReply<'_, ResolvedAuth> {
            Box::pin(async move {
                assert_eq!(
                    surface,
                    CallerSurface::CliRelay(bot_core::auth::CliVendor::Claude)
                );
                assert!(matches!(
                    source,
                    AuthSource::Credential {
                        expected_kind: CredentialKind::SubscriptionToken,
                        ..
                    }
                ));
                Ok(ResolvedAuth {
                    secret: SecretValue(b"sk-ant-oat-relay-secret".to_vec()),
                    headers: vec![("x-host-account".into(), "acct-7".into())],
                })
            })
        }
    }
    let host = ModelHost {
        providers: [(
            "sub".to_owned(),
            Provider {
                protocol: Protocol::Anthropic,
                endpoint: HostPort {
                    host: "api.anthropic.com".into(),
                    port: 443,
                },
                path: "/v1/messages".into(),
                auth: AuthSource::Credential {
                    name: SecretName::parse("claude-sub").unwrap(),
                    expected_kind: CredentialKind::SubscriptionToken,
                },
                header_mode: HeaderMode::BearerMergeHeader {
                    header: "anthropic-beta",
                    token: "oauth-2025-04-20",
                },
                headers: vec![],
                forward_headers: vec!["anthropic-version".into(), "anthropic-beta".into()],
                body_fields: vec![],
                models: [("assistant".to_owned(), "claude-sonnet-4".to_owned())].into(),
            },
        )]
        .into(),
        allow: [(ProfileId("personal".into()), ["sub".to_owned()].into())].into(),
    };
    let clock = Arc::new(ManualClock::new(1_000));
    let ledger = Arc::new(MemLedger::new(clock.clone()));
    ledger.add_run(run_context(
        "personal",
        "run-personal",
        1,
        "local:owner",
        "main",
        &["run"],
    ));
    let upstream = Arc::new(FakeUpstream::new());
    let proxy = ModelProxy::with_auth(
        host,
        ledger,
        Arc::new(CustomAuth),
        upstream.clone(),
        clock,
        ModelLimits::default(),
    );
    let mut req = request("personal", "assistant", "{}");
    req.headers = vec![
        ("anthropic-beta".into(), "claude-code-20250219".into()),
        ("authorization".into(), "Bearer spoof".into()),
    ];
    proxy.forward(req).await.unwrap();
    let sent = upstream.requests();
    let headers = &sent[0].2;
    assert!(headers.contains(&(
        "authorization".into(),
        "Bearer sk-ant-oat-relay-secret".into()
    )));
    assert!(headers.contains(&("x-host-account".into(), "acct-7".into())));
    assert!(headers.contains(&(
        "anthropic-beta".into(),
        "claude-code-20250219,oauth-2025-04-20".into()
    )));
    assert_eq!(
        headers.iter().filter(|(k, _)| k == "authorization").count(),
        1
    );
}
