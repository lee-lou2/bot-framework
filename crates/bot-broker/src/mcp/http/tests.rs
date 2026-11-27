//! Streamable HTTP client tests against `FakeMcp` and `FakeWire` for modern and legacy servers.
use super::*;
use crate::testing::{Era, FakeMcp, FakeWire, McpRequest, McpTool, Probe};
use serde_json::json;

mod hardening;

const LIMIT: Duration = Duration::from_secs(5);

fn tools() -> Vec<McpTool> {
    vec![
        McpTool::new(
            "get",
            "Query",
            json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        ),
        McpTool::new("put", "Store", json!({"type": "object"})),
    ]
}

fn client(server: &Arc<FakeMcp>, bearer: Option<&str>) -> (Http, Arc<FakeWire>) {
    let wire = Arc::new(FakeWire::new(Arc::clone(server)));
    let remote = Remote::parse("https://mcp.example/mcp", None).unwrap();
    (
        Http::new(wire.clone(), remote, bearer.map(str::to_owned), false),
        wire,
    )
}

fn server(era: Era) -> Arc<FakeMcp> {
    Arc::new(FakeMcp::new(era, tools()))
}

fn rpcs(server: &FakeMcp) -> Vec<String> {
    server
        .requests()
        .iter()
        .map(|r| {
            if r.method == "DELETE" {
                "DELETE".into()
            } else {
                r.rpc()
            }
        })
        .collect()
}

async fn round_trip(http: &mut Http) -> Value {
    http.connect(LIMIT).await.unwrap();
    assert_eq!(http.tools(LIMIT).await.unwrap().len(), 2);
    let found = http.find_tool("get", LIMIT).await.unwrap().unwrap();
    assert_eq!(found["description"], "Query");
    http.call(
        "get",
        r#"{"q":"x"}"#,
        r#""bot/call-id":"c1""#,
        Vec::new(),
        LIMIT,
    )
    .await
    .unwrap()
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

#[test]
fn the_fixed_meta_names_the_current_version() {
    assert!(MODERN_META.contains(&format!("\"{MODERN}\"")));
    assert_eq!((MODERN, LEGACY), ("2026-07-28", "2025-11-25"));
}

#[tokio::test]
async fn a_current_server_needs_no_handshake_and_answers_json_and_sse_alike() {
    for sse in [false, true] {
        let mut fake = FakeMcp::new(Era::Modern, tools());
        fake.sse = sse;
        let fake = Arc::new(fake);
        let (mut http, _) = client(&fake, None);
        let result = round_trip(&mut http).await;
        assert_eq!(text(&result), "ok:get:{\"q\":\"x\"}", "sse={sse}");
        assert_eq!(http.version(), Some(MODERN));
        // No handshake or session: discover -> list (twice: tools() + find_tool) -> call.
        assert_eq!(
            rpcs(&fake),
            ["server/discover", "tools/list", "tools/list", "tools/call"],
            "sse={sse}"
        );
        for request in fake.requests() {
            assert_eq!(request.header("mcp-protocol-version"), Some(MODERN));
            assert_eq!(request.header("mcp-method"), Some(request.rpc().as_str()));
            assert!(request.header("mcp-session-id").is_none());
        }
        let call = fake.requests().pop().unwrap();
        assert_eq!(call.header("mcp-name"), Some("get"));
        assert!(
            call.body.contains(r#""arguments":{"q":"x"}"#),
            "{}",
            call.body
        );
        let body: Value = serde_json::from_str(&call.body).unwrap();
        let meta = &body["params"]["_meta"];
        assert_eq!(meta["io.modelcontextprotocol/protocolVersion"], MODERN);
        assert_eq!(
            meta["io.modelcontextprotocol/clientCapabilities"],
            json!({})
        );
        assert_eq!(meta["bot/call-id"], "c1");
    }
}

#[tokio::test]
async fn a_legacy_server_is_reached_only_after_the_official_fallback_and_keeps_its_session() {
    for probe in [
        Probe::Bare400,
        Probe::NotFound404,
        Probe::NotAllowed405,
        Probe::Rpc200,
        Probe::Modern400Legacy,
    ] {
        for sse in [false, true] {
            let mut fake = FakeMcp::new(Era::Legacy, tools());
            fake.probe = probe;
            fake.sse = sse;
            fake.ping = sse;
            let fake = Arc::new(fake);
            let (mut http, _) = client(&fake, None);
            let result = round_trip(&mut http).await;
            http.close().await;
            assert_eq!(text(&result), "ok:get:{\"q\":\"x\"}", "{probe:?} sse={sse}");
            assert_eq!(http.version(), Some(LEGACY));
            let seen = rpcs(&fake);
            // Read-only discover goes first; initialize follows only after legacy fallback.
            assert_eq!(
                &seen[..3],
                ["server/discover", "initialize", "notifications/initialized"],
                "{probe:?}"
            );
            assert_eq!(
                seen.iter().filter(|m| *m == "tools/call").count(),
                1,
                "{probe:?}: call must happen once"
            );
            assert_eq!(seen.last().map(String::as_str), Some("DELETE"));
            if sse {
                assert!(
                    seen.iter().any(|m| m.is_empty()),
                    "{probe:?}: expected response to server ping"
                );
            }
            for request in &fake.requests()[2..] {
                assert_eq!(
                    request.header("mcp-protocol-version"),
                    Some(LEGACY),
                    "{probe:?}"
                );
                assert_eq!(
                    request.header("mcp-session-id"),
                    Some("sess-1"),
                    "{probe:?}"
                );
            }
            assert!(fake.requests()[1].header("mcp-session-id").is_none());
        }
    }
}

#[tokio::test]
async fn a_legacy_server_without_sessions_and_one_refusing_delete_both_work() {
    let mut fake = FakeMcp::new(Era::Legacy, tools());
    fake.session = None;
    fake.delete_status = 405;
    let fake = Arc::new(fake);
    let (mut http, _) = client(&fake, None);
    round_trip(&mut http).await;
    http.close().await;
    assert!(
        fake.requests()
            .iter()
            .all(|r| r.header("mcp-session-id").is_none())
    );
    assert_eq!(fake.count("DELETE"), 0);
}

#[tokio::test]
async fn a_modern_error_never_triggers_the_legacy_handshake() {
    let mut fake = FakeMcp::new(Era::Legacy, tools());
    fake.probe = Probe::Modern400None;
    let fake = Arc::new(fake);
    let (mut http, _) = client(&fake, None);
    assert_eq!(http.connect(LIMIT).await, Err(McpError::Version));
    assert_eq!(rpcs(&fake), ["server/discover"]);
}

#[tokio::test]
async fn authentication_and_server_failures_are_not_a_reason_to_fall_back() {
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.token = Some("secret-token-value".into());
    let fake = Arc::new(fake);
    let (mut http, _) = client(&fake, None);
    assert_eq!(
        http.connect(LIMIT).await,
        Err(McpError::Status {
            status: 401,
            code: None
        })
    );
    assert_eq!(
        rpcs(&fake),
        ["server/discover"],
        "auth failure must not trigger initialize"
    );
    let (mut ok, _) = client(&fake, Some("secret-token-value"));
    ok.connect(LIMIT).await.unwrap();
    let mut broken = FakeMcp::new(Era::Modern, tools());
    broken.fail = Some(("server/discover".into(), 500));
    let broken = Arc::new(broken);
    let (mut http, _) = client(&broken, None);
    assert_eq!(
        http.connect(LIMIT).await,
        Err(McpError::Status {
            status: 500,
            code: None
        })
    );
    assert_eq!(rpcs(&broken), ["server/discover"]);
}

#[tokio::test]
async fn the_bearer_rides_only_in_the_authorization_header() {
    for era in [Era::Modern, Era::Legacy] {
        let mut fake = FakeMcp::new(era, tools());
        fake.token = Some("bearer-value-123456".into());
        fake.sse = true;
        let fake = Arc::new(fake);
        let (mut http, wire) = client(&fake, Some("bearer-value-123456"));
        round_trip(&mut http).await;
        http.close().await;
        let all: Vec<McpRequest> = fake.requests();
        assert!(all.len() >= 4);
        for request in &all {
            assert_eq!(
                request.header("authorization"),
                Some("Bearer bearer-value-123456")
            );
            assert!(
                !request.body.contains("bearer-value-123456"),
                "{era:?}: body"
            );
            assert!(!request.path.contains("bearer-value-123456"));
            for (name, value) in &request.headers {
                if !name.eq_ignore_ascii_case("authorization") {
                    assert!(!value.contains("bearer-value-123456"), "{era:?}: {name}");
                }
            }
        }
        // Marked as secret header on the wire port so diagnostics redact it.
        let secrets = wire.secret_values.lock().unwrap().clone();
        assert!(!secrets.is_empty() && secrets.iter().all(|v| v == "Bearer bearer-value-123456"));
    }
}

#[tokio::test]
async fn pagination_is_followed_to_the_end_and_an_endless_list_is_an_error() {
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.page = 1;
    let fake = Arc::new(fake);
    let (mut http, _) = client(&fake, None);
    http.connect(LIMIT).await.unwrap();
    assert_eq!(http.tools(LIMIT).await.unwrap().len(), 2);
    assert_eq!(fake.count("tools/list"), 2);
    let many: Vec<McpTool> = (0..40)
        .map(|n| McpTool::new(&format!("t{n}"), "", json!({"type": "object"})))
        .collect();
    let mut endless = FakeMcp::new(Era::Modern, many);
    endless.page = 1;
    let endless = Arc::new(endless);
    let (mut http, _) = client(&endless, None);
    http.connect(LIMIT).await.unwrap();
    assert_eq!(http.tools(LIMIT).await, Err(McpError::Protocol));
    assert_eq!(endless.count("tools/list"), MAX_PAGES);
}

/// Only `tools/call` carries a wire timeout slightly longer than the call limit so long non-streaming calls do not trip the default 120s idle timeout.
#[tokio::test]
async fn only_the_call_request_carries_a_transport_limit_a_little_longer_than_the_call_limit() {
    for era in [Era::Modern, Era::Legacy] {
        let fake = server(era);
        let (mut http, wire) = client(&fake, None);
        http.connect(LIMIT).await.unwrap();
        http.tools(LIMIT).await.unwrap();
        http.call("get", "{}", "", Vec::new(), Duration::from_secs(900))
            .await
            .unwrap();
        http.close().await;
        let seen = wire.timeouts.lock().unwrap().clone();
        let calls: Vec<_> = seen.iter().filter(|(rpc, _)| rpc == "tools/call").collect();
        assert_eq!(calls.len(), 1, "{seen:?}");
        assert_eq!(
            calls[0].1,
            Some(Duration::from_secs(900) + AUX_LIMIT),
            "call limit plus margin"
        );
        assert!(
            seen.iter()
                .filter(|(rpc, _)| rpc != "tools/call")
                .all(|(_, limit)| limit.is_none()),
            "must not set wire timeout on handshake/list/close: {seen:?}"
        );
        assert!(http.wire_timeout.is_none());
    }
}

#[tokio::test]
async fn redirects_hangs_losses_and_unreachable_servers_are_classified_without_resending() {
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.redirect = Some(("tools/call".into(), "https://elsewhere.example/mcp".into()));
    let fake = Arc::new(fake);
    let (mut http, wire) = client(&fake, None);
    http.connect(LIMIT).await.unwrap();
    let err = http
        .call("get", "{}", "", Vec::new(), LIMIT)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        McpError::Status {
            status: 302,
            code: None
        }
    );
    assert_eq!(fake.count("tools/call"), 1);
    assert!(
        wire.targets
            .lock()
            .unwrap()
            .iter()
            .all(|t| t.1 == "mcp.example")
    );
    let fake = server(Era::Modern);
    let (mut http, wire) = client(&fake, None);
    http.connect(LIMIT).await.unwrap();
    *wire.lose.lock().unwrap() = Some("tools/call".into());
    assert_eq!(
        http.call("get", "{}", "", Vec::new(), LIMIT).await,
        Err(McpError::Io)
    );
    assert_eq!(fake.count("tools/call"), 1, "must not resend");
    *wire.lose.lock().unwrap() = None;
    let mut slow = FakeMcp::new(Era::Modern, tools());
    slow.hang = Some("tools/call".into());
    let slow = Arc::new(slow);
    let (mut http, wire) = client(&slow, None);
    http.connect(LIMIT).await.unwrap();
    assert_eq!(
        http.call("get", "{}", "", Vec::new(), Duration::from_millis(100))
            .await,
        Err(McpError::Timeout)
    );
    wire.unreachable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(http.connect(LIMIT).await, Err(McpError::Unreached));
}

#[tokio::test]
async fn protocol_violations_are_errors_not_guesses() {
    // Modern servers must not send server-initiated requests on the stream.
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.sse = true;
    fake.ping = true;
    let fake = Arc::new(fake);
    let (mut http, _) = client(&fake, None);
    http.connect(LIMIT).await.unwrap_err();
    assert_eq!(rpcs(&fake), ["server/discover"]);
    assert!(matches!(
        finish(Answer {
            status: 200,
            session: None,
            message: Message::Empty
        }),
        Err(McpError::Protocol)
    ));
    let input_required = json!({"resultType": "input_required", "inputRequests": {}});
    assert!(matches!(
        finish(Answer {
            status: 200,
            session: None,
            message: Message::Result(input_required)
        }),
        Err(McpError::Protocol)
    ));
    let complete = json!({"resultType": "complete", "content": []});
    assert!(
        finish(Answer {
            status: 200,
            session: None,
            message: Message::Result(complete)
        })
        .is_ok()
    );
}

#[tokio::test]
async fn schema_annotated_arguments_are_mirrored_into_headers_and_a_mismatch_is_refused() {
    let schema = json!({"type": "object", "properties": {"region": {"type": "string", "x-mcp-header": "Region"}}});
    let fake = Arc::new(FakeMcp::new(
        Era::Modern,
        vec![McpTool::new("sql", "", schema.clone())],
    ));
    let (mut http, _) = client(&fake, None);
    http.connect(LIMIT).await.unwrap();
    let params =
        crate::mcp::header::param_headers(&schema, &json!({"region": "us-west1"})).unwrap();
    http.call("sql", r#"{"region":"us-west1"}"#, "", params, LIMIT)
        .await
        .unwrap();
    let sent = fake.requests().pop().unwrap();
    assert_eq!(sent.header("mcp-param-region"), Some("us-west1"));
    let err = http
        .call("sql", r#"{"region":"us-west1"}"#, "", Vec::new(), LIMIT)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        McpError::Status {
            status: 400,
            code: Some(-32020)
        }
    );
    assert_eq!(fake.count("tools/call"), 2);
}

#[tokio::test]
async fn the_older_legacy_versions_negotiate_and_only_the_newer_two_get_the_version_header() {
    for version in ["2025-11-25", "2025-06-18", "2025-03-26"] {
        let mut fake = FakeMcp::new(Era::Legacy, tools());
        fake.legacy = version;
        let fake = Arc::new(fake);
        let (mut http, _wire) = client(&fake, None);
        let result = round_trip(&mut http).await;
        assert!(result.to_string().contains("ok:get"), "{version}: {result}");
        assert_eq!(http.version(), Some(version), "{version}");
        http.close().await;
        // 2025-03-26 predates the `MCP-Protocol-Version` request header.
        let requests = fake.requests();
        let after: Vec<&McpRequest> = requests
            .iter()
            .filter(|r| !matches!(r.rpc().as_str(), "server/discover" | "initialize"))
            .collect();
        assert!(after.len() >= 4, "{version}: {after:?}");
        for r in &after {
            assert_eq!(
                r.header("mcp-session-id"),
                Some("sess-1"),
                "{version}: {r:?}"
            );
            let sent = r.header("mcp-protocol-version");
            assert_eq!(
                sent,
                (version != "2025-03-26").then_some(version),
                "{version}: {r:?}"
            );
        }
        let init = requests.iter().find(|r| r.rpc() == "initialize").unwrap();
        assert!(
            init.body.contains("\"protocolVersion\":\"2025-11-25\""),
            "{}",
            init.body
        );
        assert_eq!(fake.count("tools/call"), 1, "{version}");
    }
}

#[tokio::test]
async fn an_older_only_server_is_reached_from_the_discover_error_and_an_unknown_version_is_refused()
{
    let mut fake = FakeMcp::new(Era::Legacy, tools());
    fake.probe = Probe::Modern400Older;
    fake.legacy = "2025-06-18";
    let fake = Arc::new(fake);
    let (mut http, _wire) = client(&fake, None);
    round_trip(&mut http).await;
    assert_eq!(http.version(), Some("2025-06-18"));
    for version in ["2024-11-05", "2099-01-01", "2025-11-26"] {
        let mut fake = FakeMcp::new(Era::Legacy, tools());
        fake.legacy = version;
        let fake = Arc::new(fake);
        let (mut http, _wire) = client(&fake, None);
        assert!(
            matches!(http.connect(LIMIT).await, Err(McpError::Version)),
            "{version}"
        );
        assert_eq!(fake.count("tools/call"), 0);
    }
}
