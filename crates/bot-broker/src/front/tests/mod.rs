use super::*;

mod cli;
mod delegate;
mod delegate_refusal;
mod limit_cause;
mod limits;
mod mcp;
mod multi_protocol;
mod observe;
mod refusal;
mod refuse_all;
mod revoked_wait;
use crate::{
    model::{ModelHost, ModelLimits, Provider},
    ports::Ran,
    rate::RateConfig,
    resolve::HostPort,
    service::{BrokerService, Config, Parts},
    testing::*,
};
use bot_core::{id::ClaimEpoch, state::Risk};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

pub(super) const KEY: &str = "provider-key-for-personal";

pub(super) struct Rig {
    pub(super) clock: Arc<ManualClock>,
    pub(super) front: ModelFront,
    pub(super) sessions: Arc<SessionTable>,
    pub(super) upstream: Arc<FakeUpstream>,
    pub(super) runner: Arc<FakeRunner>,
    pub(super) ledger: Arc<MemLedger>,
    pub(super) catalog: Arc<FakeCatalog>,
    pub(super) turns: Arc<RecordingTurns>,
    pub(super) delegation: Arc<FakeDelegation>,
    pub(super) park: Arc<RecordingPark>,
    pub(super) token: String,
    pub(super) attempt: Attempt,
}

pub(super) fn world() -> Arc<bot_core::profile::Snapshot> {
    snapshot("personal", "r1")
        .role("owner", &["run", "echo.use", "echo.drop"])
        .member("local:owner", &["owner"])
        .policy_scopes(&["run", "echo.use", "echo.drop"])
        .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
        .tool(
            "main",
            "echo.drop",
            tool(Risk::Irreversible, &["echo.drop"]),
        )
        .tool("main", "echo.locked", tool(Risk::Read, &["echo.admin"]))
        .build()
}

pub(super) async fn rig(
    front: impl FnOnce(&mut FrontConfig),
    limits: impl FnOnce(&mut ModelLimits),
) -> Rig {
    rig_with(front, limits, None).await
}

/// Factory for overriding `CallBook` (default: `DirectIds`) using the test ledger and broker.
pub(super) type MakeCalls =
    Box<dyn FnOnce(&Arc<MemLedger>, &Arc<BrokerService>) -> Arc<dyn CallBook>>;

/// Factory for wrapping `Broker` while keeping the real `BrokerService` for preflight and `CallBook`.
pub(super) type MakeBroker = Box<dyn FnOnce(Arc<BrokerService>) -> Arc<dyn Broker>>;

pub(super) async fn rig_with(
    front: impl FnOnce(&mut FrontConfig),
    limits: impl FnOnce(&mut ModelLimits),
    calls: Option<MakeCalls>,
) -> Rig {
    rig_full(front, limits, calls, Config::default(), None).await
}

pub(super) async fn rig_full(
    front: impl FnOnce(&mut FrontConfig),
    limits: impl FnOnce(&mut ModelLimits),
    calls: Option<MakeCalls>,
    broker_config: Config,
    wrap: Option<MakeBroker>,
) -> Rig {
    let clock = Arc::new(ManualClock::new(1_000));
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(world());
    let ledger = Arc::new(MemLedger::new(clock.clone()));
    ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &["run", "echo.use", "echo.drop"],
    ));
    ledger.set_active_revision("personal", "r1");
    let runner = Arc::new(FakeRunner::new());
    let broker = Arc::new(
        BrokerService::new(
            Parts {
                catalog: catalog.clone(),
                memory: Arc::new(MemMemory::new(ledger.clone())),
                ledger: ledger.clone(),
                effects: Arc::new(FakeEffects::new()),
                audit: Arc::new(MemAudit::new()),
                runner: runner.clone(),
                sealer: Arc::new(FakeSealer),
                tracer: Arc::new(RecordingTracer::new()),
                clock: clock.clone(),
                entropy: Arc::new(CountingEntropy::default()),
            },
            broker_config,
        )
        .with_schedules(Arc::new(MemSchedules::new(ledger.clone()))),
    );
    let host = ModelHost {
        providers: [(
            "main".to_owned(),
            Provider {
                protocol: Protocol::Anthropic,
                endpoint: HostPort {
                    host: "api.main.example".into(),
                    port: 443,
                },
                path: "/v1/messages".into(),
                auth: bot_core::auth::AuthSource::Secret(SecretName::parse("model-key").unwrap()),
                header_mode: bot_core::auth::HeaderMode::ApiKey("x-api-key"),
                headers: vec![],
                forward_headers: vec!["anthropic-version".into(), "anthropic-beta".into()],
                body_fields: vec![],
                models: [("assistant".to_owned(), "model-large".to_owned())].into(),
            },
        )]
        .into(),
        allow: [(ProfileId("personal".into()), ["main".to_owned()].into())].into(),
    };
    let upstream = Arc::new(FakeUpstream::new());
    let mut model_limits = ModelLimits::default();
    limits(&mut model_limits);
    let model = Arc::new(ModelProxy::new(
        host,
        ledger.clone(),
        Arc::new(FakeSecrets::new(&[("personal", "model-key", KEY)])),
        upstream.clone(),
        clock.clone(),
        model_limits,
    ));
    let sessions = SessionTable::new(Arc::new(CountingEntropy::default()));
    let attempt = Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        generation: Generation(1),
        claim: Some(ClaimEpoch(1)),
    };
    let token = sessions
        .issue(Session {
            attempt: attempt.clone(),
            agent: AgentId("main".into()),
            alias: "assistant".into(),
            protocol: Protocol::Anthropic,
        })
        .unwrap();
    let turns = Arc::new(RecordingTurns::new());
    // Mirror refusal markers into run checkpoints where `CallBook::marked` reads them.
    turns.mirror_into(ledger.clone());
    let delegation = Arc::new(FakeDelegation::new());
    let park = Arc::new(RecordingPark::new());
    let mut cfg = FrontConfig::default();
    front(&mut cfg);
    let front = ModelFront::start(
        "127.0.0.1:0".parse().unwrap(),
        FrontParts {
            sessions: sessions.clone(),
            model,
            broker: match wrap {
                Some(wrap) => wrap(broker.clone()),
                None => broker.clone(),
            },
            catalog: catalog.clone(),
            calls: match calls {
                Some(make) => make(&ledger, &broker),
                None => Arc::new(DirectIds),
            },
            turns: turns.clone(),
            ledger: ledger.clone(),
            delegation: delegation.clone(),
            park: park.clone(),
        },
        cfg,
    )
    .await
    .unwrap();
    Rig {
        clock,
        front,
        sessions,
        upstream,
        runner,
        ledger,
        catalog,
        turns,
        delegation,
        park,
        token,
        attempt,
    }
}

pub(super) async fn plain() -> Rig {
    rig(|_| {}, |_| {}).await
}

pub(super) struct Reply {
    pub(super) status: u16,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: String,
}

fn dechunk(raw: &[u8]) -> Vec<u8> {
    let (mut out, mut rest) = (Vec::new(), raw);
    loop {
        let Some(at) = rest.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size = usize::from_str_radix(std::str::from_utf8(&rest[..at]).unwrap(), 16).unwrap();
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&rest[at + 2..at + 2 + size]);
        rest = &rest[at + 2 + size + 2..];
    }
}

pub(super) async fn raw(addr: SocketAddr, request: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut all = Vec::new();
    let _ = timeout(Duration::from_secs(10), stream.read_to_end(&mut all)).await;
    let split = all
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(all.len(), |i| i + 4);
    let head = String::from_utf8_lossy(&all[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap_or_default()
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let body = &all[split..];
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v == "chunked");
    let body = String::from_utf8_lossy(&if chunked {
        dechunk(body)
    } else {
        body.to_vec()
    })
    .into_owned();
    Reply {
        status,
        headers,
        body,
    }
}

pub(super) fn post(path: &str, token: Option<&str>, header: &str, body: &str) -> Vec<u8> {
    let auth = token
        .map(|t| format!("{header}: {t}\r\n"))
        .unwrap_or_default();
    format!("POST {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

pub(super) fn messages(rig: &Rig, body: &str) -> Vec<u8> {
    post(
        "/v1/messages?beta=true",
        Some(&rig.token),
        "x-api-key",
        body,
    )
}

pub(super) async fn rpc(rig: &Rig, body: &str) -> Reply {
    let token = format!("Bearer {}", rig.token);
    raw(
        rig.front.addr(),
        &post("/mcp", Some(&token), "authorization", body),
    )
    .await
}

pub(super) fn sse() -> [&'static [u8]; 3] {
    [
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s1\"}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hel",
        b"lo\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]
}

#[tokio::test]
async fn a_streaming_model_request_is_relayed_as_sse_with_the_providers_key_not_the_sessions() {
    let r = plain().await;
    r.upstream.respond_chunks(200, &sse(), End::Eof);
    let body = r#"{"model":"attacker","stream":true,"messages":[]}"#;
    let mut request = messages(&r, body);
    let extra = b"anthropic-version: 2023-06-01\r\nanthropic-beta: claude-code-20250219\r\n";
    let at = request.windows(2).position(|w| w == b"\r\n").unwrap() + 2;
    request.splice(at..at, extra.iter().copied());
    let reply = raw(r.front.addr(), &request).await;
    assert_eq!(reply.status, 200);
    assert!(reply.headers.contains(&(
        "content-type".into(),
        "text/event-stream; charset=utf-8".into()
    )));
    assert!(
        reply
            .headers
            .contains(&("transfer-encoding".into(), "chunked".into()))
    );
    assert_eq!(
        reply.body,
        sse()
            .concat()
            .iter()
            .map(|b| *b as char)
            .collect::<String>()
    );
    let (endpoint, path, headers, sent) = &r.upstream.requests()[0];
    assert_eq!(
        (endpoint.as_str(), path.as_str()),
        ("api.main.example:443", "/v1/messages")
    );
    assert!(headers.contains(&("x-api-key".into(), KEY.into())));
    assert!(headers.contains(&("anthropic-version".into(), "2023-06-01".into())));
    assert!(headers.contains(&("anthropic-beta".into(), "claude-code-20250219".into())));
    assert!(
        headers.iter().all(|(_, v)| !v.contains(&r.token)),
        "session token must not be forwarded upstream"
    );
    assert!(sent.contains("\"model\":\"model-large\""), "{sent}");
}

#[tokio::test]
async fn a_plain_model_request_gets_one_json_answer() {
    let r = plain().await;
    let reply = raw(r.front.addr(), &messages(&r, r#"{"messages":[]}"#)).await;
    assert_eq!((reply.status, reply.body.as_str()), (200, "{\"ok\":true}"));
    assert!(
        reply
            .headers
            .contains(&("content-type".into(), "application/json".into()))
    );
}

#[tokio::test]
async fn a_mid_stream_leak_ends_the_stream_with_an_error_event_and_the_key_never_reaches_the_client()
 {
    let r = plain().await;
    let (head, tail) = KEY.split_at(9);
    let events = [
        format!("data: {{\"delta\":{{\"text\":\"{head}\"}}}}\n\n"),
        format!("data: {{\"delta\":{{\"text\":\"{tail}\"}}}}\n\n"),
    ];
    r.upstream.respond_chunks(
        200,
        &[
            events[0].as_bytes(),
            events[1].as_bytes(),
            b"data: {\"after\":1}\n\n",
        ],
        End::Eof,
    );
    let reply = raw(r.front.addr(), &messages(&r, r#"{"stream":true}"#)).await;
    assert_eq!(reply.status, 200);
    assert!(
        !reply.body.contains(tail) && !reply.body.contains(KEY) && !reply.body.contains("after"),
        "{}",
        reply.body
    );
    assert!(reply.body.contains("event: error") && reply.body.contains("blocked"));
}

#[tokio::test]
async fn an_upstream_error_status_is_passed_on_as_a_fixed_json_body() {
    let r = plain().await;
    r.upstream.respond_chunks(
        429,
        &[b"{\"error\":\"account acct-9 over quota\"}"],
        End::Eof,
    );
    let reply = raw(r.front.addr(), &messages(&r, r#"{"stream":true}"#)).await;
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (429, r#"{"error":{"status":429}}"#)
    );
}

#[tokio::test]
async fn requests_without_a_valid_session_never_reach_the_model_or_the_tools() {
    let r = plain().await;
    for request in [
        post("/v1/messages", None, "x-api-key", r#"{"stream":true}"#),
        post(
            "/v1/messages",
            Some("bot-s-0000"),
            "x-api-key",
            r#"{"stream":true}"#,
        ),
        post(
            "/v1/messages",
            Some(&format!("Bearer {}x", r.token)),
            "authorization",
            r#"{}"#,
        ),
        post(
            "/mcp",
            None,
            "authorization",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        ),
        post(
            "/mcp",
            Some("Bearer nope"),
            "authorization",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        ),
    ] {
        let reply = raw(r.front.addr(), &request).await;
        assert_eq!(reply.status, 401, "{}", String::from_utf8_lossy(&request));
        assert_eq!(
            reply.body,
            error_body("authentication_error", "unauthorized")
        );
        assert!(!reply.body.contains(&r.token));
    }
    assert!(r.upstream.requests().is_empty() && r.runner.jobs().is_empty());
    // Both `Authorization: Bearer` and `x-api-key` authenticate the same session.
    let by_bearer = post(
        "/v1/messages",
        Some(&format!("Bearer {}", r.token)),
        "authorization",
        r#"{"messages":[]}"#,
    );
    assert_eq!(raw(r.front.addr(), &by_bearer).await.status, 200);
}

#[tokio::test]
async fn a_revoked_session_stops_working_and_so_does_a_run_that_changed_hands() {
    let r = plain().await;
    assert_eq!(
        raw(r.front.addr(), &messages(&r, r#"{"messages":[]}"#))
            .await
            .status,
        200
    );
    r.ledger.bump_claim("r1");
    let stale = raw(r.front.addr(), &messages(&r, r#"{"messages":[]}"#)).await;
    assert_eq!(
        (stale.status, stale.body.as_str()),
        (403, error_body("permission_error", "stale").as_str())
    );
    let call = rpc(&r, r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo_say","arguments":{"a":1},"_meta":{"claudecode/toolUseId":"toolu_s1"}}}"#).await;
    assert!(
        call.body.contains("stale") && call.body.contains("isError\":true"),
        "{}",
        call.body
    );
    assert!(r.runner.jobs().is_empty());
    assert_eq!(
        r.upstream.requests().len(),
        1,
        "stale run request must not reach upstream"
    );
    let fresh = Attempt {
        claim: Some(r.ledger.claim_epoch("r1")),
        ..r.attempt.clone()
    };
    let token = r
        .sessions
        .issue(Session {
            attempt: fresh,
            agent: AgentId("main".into()),
            alias: "assistant".into(),
            protocol: Protocol::Anthropic,
        })
        .unwrap();
    let ok = raw(
        r.front.addr(),
        &post(
            "/v1/messages",
            Some(&token),
            "x-api-key",
            r#"{"messages":[]}"#,
        ),
    )
    .await;
    assert_eq!(ok.status, 200);
    // Revoking another claim's attempt must not revoke this claim's token.
    let other = Attempt {
        claim: Some(ClaimEpoch(99)),
        ..r.attempt.clone()
    };
    r.sessions.revoke_attempt(&other);
    assert_eq!(
        raw(
            r.front.addr(),
            &post(
                "/v1/messages",
                Some(&token),
                "x-api-key",
                r#"{"messages":[]}"#
            )
        )
        .await
        .status,
        200
    );
    r.sessions.revoke_attempt(&Attempt {
        claim: Some(r.ledger.claim_epoch("r1")),
        ..r.attempt.clone()
    });
    assert_eq!(
        raw(
            r.front.addr(),
            &post(
                "/v1/messages",
                Some(&token),
                "x-api-key",
                r#"{"messages":[]}"#
            )
        )
        .await
        .status,
        401
    );
    r.sessions.revoke(&r.token);
}
