//! Streamable HTTP 클라이언트: 가짜 서버(`FakeMcp`, 명세의 요구를 검사한다)와 가짜 전송 위에서 현행·구판 서버를 각각 확인한다.
use super::*;
use crate::testing::{Era, FakeMcp, FakeWire, McpRequest, McpTool, Probe};
use serde_json::json;

mod hardening;

const LIMIT: Duration = Duration::from_secs(5);

fn tools() -> Vec<McpTool> {
    vec![
        McpTool::new(
            "get",
            "조회",
            json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        ),
        McpTool::new("put", "저장", json!({"type": "object"})),
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
    assert_eq!(found["description"], "조회");
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
        // 핸드셰이크도 세션도 없다: discover → list(두 번: tools()·find_tool) → call
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
        // 호출 본문의 `_meta`: 필수 판본·기능과 우리가 더 싣는 멱등 키, 인자는 받은 바이트 그대로
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
            // 읽기 전용 discover가 먼저이고, 구판 확인 뒤에만 initialize가 간다. 서버가 보낸 `ping`에는 응답(메서드 없는 본문)이 간다
            assert_eq!(
                &seen[..3],
                ["server/discover", "initialize", "notifications/initialized"],
                "{probe:?}"
            );
            assert_eq!(
                seen.iter().filter(|m| *m == "tools/call").count(),
                1,
                "{probe:?}: 호출은 한 번"
            );
            assert_eq!(seen.last().map(String::as_str), Some("DELETE"));
            if sse {
                assert!(
                    seen.iter().any(|m| m.is_empty()),
                    "{probe:?}: ping에 답했다"
                );
            }
            // initialize 뒤의 모든 요청에 판본과 세션 ID가 있다
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
    // 세션이 없으면 DELETE도 보내지 않는다
    assert_eq!(fake.count("DELETE"), 0);
}

#[tokio::test]
async fn a_modern_error_never_triggers_the_legacy_handshake() {
    // 현행 서버가 «지원하는 판본이 없다»고 답했다 → 판본 오류, 구판으로 떨어지지 않는다
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
        "인증 실패에 initialize를 보내지 않는다"
    );
    // 맞는 자격이면 된다
    let (mut ok, _) = client(&fake, Some("secret-token-value"));
    ok.connect(LIMIT).await.unwrap();
    // 500은 판본 문제가 아니다
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
                "{era:?}: 본문"
            );
            assert!(!request.path.contains("bearer-value-123456"));
            for (name, value) in &request.headers {
                if !name.eq_ignore_ascii_case("authorization") {
                    assert!(!value.contains("bearer-value-123456"), "{era:?}: {name}");
                }
            }
        }
        // 전송 포트에는 비밀 머리로 표시돼 넘어간다(진단 출력에서 가려진다)
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
    // 쪽이 끝나지 않으면 불완전한 목록이다
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

#[tokio::test]
async fn redirects_hangs_losses_and_unreachable_servers_are_classified_without_resending() {
    // 3xx는 따라가지 않고 상태로 돌려준다(호출은 한 번뿐)
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
    // 응답을 잃으면 Io, 끝나지 않으면 Timeout, 연결 전이면 Unreached
    let fake = server(Era::Modern);
    let (mut http, wire) = client(&fake, None);
    http.connect(LIMIT).await.unwrap();
    *wire.lose.lock().unwrap() = Some("tools/call".into());
    assert_eq!(
        http.call("get", "{}", "", Vec::new(), LIMIT).await,
        Err(McpError::Io)
    );
    assert_eq!(fake.count("tools/call"), 1, "다시 보내지 않는다");
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
    // 현행 서버가 스트림에서 요청을 보내면 위반이다(`input_required`·서버 요청은 지원하지 않는다)
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.sse = true;
    fake.ping = true;
    let fake = Arc::new(fake);
    let (mut http, _) = client(&fake, None);
    http.connect(LIMIT).await.unwrap_err();
    assert_eq!(rpcs(&fake), ["server/discover"]);
    // 응답 본문이 없는 2xx, 알 수 없는 결과 종류
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
    // 머리 없이 보내면 서버가 HeaderMismatch로 거절하고, 클라이언트는 다시 보내지 않는다
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
        // 핸드셰이크 뒤의 요청: 세션 ID는 모두, 판본 머리는 2025-06-18부터(2025-03-26 명세에는 그 머리가 없다)
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
        // initialize는 우리가 내세우는 최신 구판으로 가고, 쓰기일 수 있는 호출은 한 번만 갔다
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
    // 현행 오류가 알려 주는 지원 판본이 더 오래된 구판뿐이어도 그 구판으로 간다
    let mut fake = FakeMcp::new(Era::Legacy, tools());
    fake.probe = Probe::Modern400Older;
    fake.legacy = "2025-06-18";
    let fake = Arc::new(fake);
    let (mut http, _wire) = client(&fake, None);
    round_trip(&mut http).await;
    assert_eq!(http.version(), Some("2025-06-18"));
    // 서버가 아는 구판이 아닌 판본으로 답하면 협상하지 않는다(도구는 부르지 않는다)
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
