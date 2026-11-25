//! 원격 Streamable HTTP MCP를 실제 소켓·운영 전송(`ReqwestWire`) 위에서 확인한다: 두 판본의 협상과 JSON·SSE 응답, 인증 머리, 리다이렉트 거절,
//! 연결 전 거절, 시간 초과·재전송 없음, 그리고 `bot mcp sync`(선언한 도구만·결정적·자격 노출 검사·접속 전 거절).
//! 서버는 `FakeMcp`(공식 규격을 검사하는 대역)를 HTTP로 내보낸 로컬 서버뿐이다. 외부 서비스·실제 키는 쓰지 않는다.
mod mcp_http_support;

use bot_broker::{
    mcp::{
        McpError,
        http::{Http, LEGACY, MODERN},
        remote::Remote,
        wire::{RemoteWire, WireFail, WireMethod, WireRequest},
    },
    proxy::SystemResolver,
    testing::{Era, FakeMcp, FakeSecrets, McpTool},
};
use bot_cli::{mcp_sync, serve::remote::ReqwestWire, settings::Settings};
use bot_core::{mcp::definition_sha256, port::SecretStore};
use mcp_http_support::{
    Counter, Served, closed_port, copy_example_profile, host_file, serve, settings,
};
use serde_json::{Value, json};
use std::{
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const LIMIT: Duration = Duration::from_secs(10);
const TOKEN: &str = "tok-remote-1";
const META: &str = "\"bot/call-id\":\"c-1\"";

fn schema() -> Value {
    json!({"type": "object", "properties": {"city": {"type": "string"}},
           "required": ["city"], "additionalProperties": false})
}

fn forecast() -> McpTool {
    McpTool::new("get-forecast", "도시의 예보", schema())
}

fn put_city() -> McpTool {
    McpTool::new("put-city", "도시를 등록한다", schema())
}

/// 서버가 내세우는 도구는 셋이고 스킬이 선언하는 것은 둘이다.
fn server_tools() -> Vec<McpTool> {
    vec![
        put_city(),
        McpTool::new("internal-admin", "운영자 전용", json!({"type": "object"})),
        forecast(),
    ]
}

fn wire() -> Arc<ReqwestWire> {
    Arc::new(ReqwestWire::new(Arc::new(SystemResolver)).expect("HTTPS 클라이언트를 만든다"))
}

fn client(server: &Served, bearer: Option<&str>) -> Http {
    let remote = Remote::parse(&server.url(), None).unwrap();
    Http::new(wire(), remote, bearer.map(str::to_owned), false)
}

async fn call(http: &mut Http) -> Result<Value, McpError> {
    http.call(
        "get-forecast",
        r#"{"city":"Seoul"}"#,
        META,
        Vec::new(),
        LIMIT,
    )
    .await
}

#[tokio::test]
async fn both_protocol_versions_round_trip_as_json_and_as_event_streams() {
    for era in [Era::Modern, Era::Legacy] {
        for sse in [false, true] {
            let mut fake = FakeMcp::new(era, vec![forecast()]);
            fake.sse = sse;
            let server = serve(fake).await;
            let mut http = client(&server, None);
            let at = format!("{era:?} sse={sse}");
            http.connect(LIMIT)
                .await
                .unwrap_or_else(|e| panic!("{at}: {e:?}"));
            let version = if era == Era::Modern { MODERN } else { LEGACY };
            assert_eq!(http.version(), Some(version), "{at}");
            assert_eq!(http.tools(LIMIT).await.unwrap().len(), 1, "{at}");
            let found = http
                .find_tool("get-forecast", LIMIT)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(found["description"], "도시의 예보", "{at}");
            let result = call(&mut http)
                .await
                .unwrap_or_else(|e| panic!("{at}: {e:?}"));
            assert!(
                result.to_string().contains("ok:get-forecast"),
                "{at}: {result}"
            );
            http.close().await;
            // 쓰기 호출은 정확히 한 번 가고, 구판만 세션을 정리한다
            assert_eq!(server.mcp.count("tools/call"), 1, "{at}");
            assert_eq!(
                server.mcp.count("DELETE"),
                usize::from(era == Era::Legacy),
                "{at}"
            );
        }
    }
}

#[tokio::test]
async fn the_bearer_travels_only_in_the_authorization_header() {
    for era in [Era::Modern, Era::Legacy] {
        let mut fake = FakeMcp::new(era, vec![forecast()]);
        fake.token = Some(TOKEN.into());
        let server = serve(fake).await;
        let mut http = client(&server, Some(TOKEN));
        http.connect(LIMIT).await.unwrap();
        let result = call(&mut http).await.unwrap();
        http.close().await;
        assert!(
            !result.to_string().contains(TOKEN),
            "{era:?}: 결과에 자격이 섞였어요"
        );
        let requests = server.mcp.requests();
        assert!(!requests.is_empty());
        for request in &requests {
            let expected = format!("Bearer {TOKEN}");
            assert_eq!(
                request.header("authorization"),
                Some(expected.as_str()),
                "{era:?}"
            );
            assert!(
                !request.body.contains(TOKEN),
                "{era:?}: 본문에 자격이 실렸어요"
            );
            assert!(
                !request.path.contains(TOKEN),
                "{era:?}: 주소에 자격이 실렸어요"
            );
            let elsewhere: Vec<&str> = request
                .headers
                .iter()
                .filter(|(name, value)| {
                    !name.eq_ignore_ascii_case("authorization") && value.contains(TOKEN)
                })
                .map(|(name, _)| name.as_str())
                .collect();
            assert!(
                elsewhere.is_empty(),
                "{era:?}: 다른 머리에 자격이 실렸어요: {elsewhere:?}"
            );
        }
        // 틀린 자격·자격 없음은 서버가 거절하고 도구는 불리지 않는다
        for wrong in [Some("wrong"), None] {
            let error = client(&server, wrong).connect(LIMIT).await.unwrap_err();
            assert!(
                matches!(error, McpError::Status { status: 401, .. }),
                "{era:?}: {error:?}"
            );
        }
        assert_eq!(server.mcp.count("tools/call"), 1);
    }
}

#[tokio::test]
async fn a_redirect_is_never_followed_or_resent() {
    let target = Counter::new().await;
    for rpc in ["tools/call", "tools/list"] {
        let mut fake = FakeMcp::new(Era::Modern, vec![forecast()]);
        fake.token = Some(TOKEN.into());
        fake.redirect = Some((
            rpc.into(),
            format!("http://127.0.0.1:{}/stolen", target.port),
        ));
        let server = serve(fake).await;
        let mut http = client(&server, Some(TOKEN));
        http.connect(LIMIT).await.unwrap();
        let error = if rpc == "tools/call" {
            call(&mut http).await.unwrap_err()
        } else {
            http.tools(LIMIT).await.unwrap_err()
        };
        assert!(
            matches!(error, McpError::Status { status: 302, .. }),
            "{rpc}: {error:?}"
        );
        assert_eq!(
            server.mcp.count(rpc),
            1,
            "{rpc}: 같은 요청을 다시 보내지 않아요"
        );
    }
    assert_eq!(
        target.accepted().await,
        0,
        "리다이렉트 목적지에는 연결하지 않아요"
    );
}

#[tokio::test]
async fn plain_http_to_anything_but_a_loopback_literal_is_refused_before_any_connection() {
    let target = Counter::new().await;
    let wire = wire();
    let request = |tls: bool, host: &str, allow_private: bool| WireRequest {
        method: WireMethod::Post,
        tls,
        host: host.into(),
        port: target.port,
        path: "/mcp".into(),
        headers: Vec::new(),
        body: b"{}".to_vec(),
        allow_private,
    };
    // 평문 http: 루프백이 아닌 리터럴·이름은 전역 사설망 허용으로도 열리지 않는다
    for (host, allow_private) in [
        ("10.0.0.5", false),
        ("10.0.0.5", true),
        ("93.184.216.34", true),
        ("localhost", false),
        ("localhost", true),
        ("mcp.example", true),
    ] {
        let result = wire.send(request(false, host, allow_private)).await;
        assert!(
            matches!(result, Err(WireFail::Unreached)),
            "http {host} allow_private={allow_private}"
        );
    }
    // https: 루프백·사설·링크로컬 리터럴은 사설망을 열지 않으면 거절한다
    for host in ["127.0.0.1", "10.0.0.5", "169.254.169.254", "::1"] {
        let result = wire.send(request(true, host, false)).await;
        assert!(matches!(result, Err(WireFail::Unreached)), "https {host}");
    }
    assert_eq!(
        target.accepted().await,
        0,
        "거절한 요청은 연결을 만들지 않아요"
    );
}

#[tokio::test]
async fn an_unreachable_server_got_nothing_and_a_silent_one_is_asked_exactly_once() {
    let dead = closed_port().await;
    let remote = Remote::parse(&format!("http://127.0.0.1:{dead}/mcp"), None).unwrap();
    let mut http = Http::new(wire(), remote, None, false);
    assert!(matches!(
        http.connect(LIMIT).await,
        Err(McpError::Unreached)
    ));

    let mut fake = FakeMcp::new(Era::Modern, vec![forecast()]);
    fake.hang = Some("tools/call".into());
    let server = serve(fake).await;
    let mut http = client(&server, None);
    http.connect(LIMIT).await.unwrap();
    let started = Instant::now();
    let error = http
        .call(
            "get-forecast",
            r#"{"city":"Seoul"}"#,
            META,
            Vec::new(),
            Duration::from_millis(400),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Timeout), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        server.mcp.count("tools/call"),
        1,
        "기다리다 끝나도 다시 보내지 않아요"
    );
}

/// 프로필 `personal`의 스킬 `weather`(선언한 도구 둘)와 그 서버를 가리키는 호스트 설정.
struct World {
    tmp: tempfile::TempDir,
    settings: Settings,
    skill: PathBuf,
    lock: PathBuf,
}

fn world(port: u16, egress: &str, auth: bool) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("profiles");
    copy_example_profile(&root);
    let skill = root.join("personal/agents/main/skills/weather");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "예보 스킬\n").unwrap();
    std::fs::write(
        skill.join("tools.yaml"),
        format!(
            "version: 1\nserver: weather\ntools:\n  - id: get-forecast\n    class: read\n    scopes: [weather.read]\n    egress: [\"{egress}\"]\n  - id: put-city\n    class: write\n    scopes: [weather.write]\n    egress: [\"{egress}\"]\n"
        ),
    )
    .unwrap();
    let host = host_file(tmp.path(), port, auth);
    let settings = settings(&root, &host);
    World {
        lock: skill.join("tools.lock.yaml"),
        skill,
        settings,
        tmp,
    }
}

async fn sync(world: &World) -> Result<(), String> {
    mcp_sync::run_with(&world.settings, &world.skill, || {
        let store = FakeSecrets::new(&[("personal", "weather-token", TOKEN)]);
        Ok(Arc::new(store) as Arc<dyn SecretStore>)
    })
    .await
}

fn read(world: &World) -> String {
    std::fs::read_to_string(&world.lock).unwrap()
}

async fn authed(era: Era) -> (Served, World) {
    let mut fake = FakeMcp::new(era, server_tools());
    fake.token = Some(TOKEN.into());
    let server = serve(fake).await;
    let world = world(server.port, &format!("127.0.0.1:{}", server.port), true);
    (server, world)
}

#[tokio::test]
async fn sync_records_only_the_declared_tools_in_a_stable_file_and_the_bearer_stays_in_the_header()
{
    for era in [Era::Modern, Era::Legacy] {
        let (server, w) = authed(era).await;
        sync(&w).await.unwrap_or_else(|e| panic!("{era:?}: {e}"));
        let first = read(&w);
        assert!(
            first.contains("get-forecast") && first.contains("put-city"),
            "{first}"
        );
        assert!(
            !first.contains("internal-admin"),
            "선언하지 않은 도구는 적지 않아요\n{first}"
        );
        let hash = definition_sha256("get-forecast", "도시의 예보", &schema()).unwrap();
        assert!(first.contains(&hash), "{first}");
        assert!(!first.contains(TOKEN), "파일에 자격이 없어요");
        // 같은 서버 상태면 바이트가 같고 파일을 다시 쓰지도 않는다
        let inode = std::fs::metadata(&w.lock).unwrap().ino();
        sync(&w).await.unwrap();
        assert_eq!(read(&w), first);
        assert_eq!(
            std::fs::metadata(&w.lock).unwrap().ino(),
            inode,
            "{era:?}: 바뀐 게 없으면 쓰지 않아요"
        );
        // 서버가 도구를 다른 순서로 내세워도 같은 파일이다
        server
            .mcp
            .set_tools(server_tools().into_iter().rev().collect());
        sync(&w).await.unwrap();
        assert_eq!(read(&w), first);
        // 설명이 바뀐 서버는 해시가 다른 파일이 된다
        let described = McpTool::new("get-forecast", "다른 설명", schema());
        server.mcp.set_tools(vec![described, put_city()]);
        sync(&w).await.unwrap();
        assert_ne!(read(&w), first);
        // 자격은 모든 요청의 인증 머리에만 실렸다
        let expected = format!("Bearer {TOKEN}");
        for request in server.mcp.requests() {
            assert_eq!(request.header("authorization"), Some(expected.as_str()));
            assert!(!request.body.contains(TOKEN));
        }
    }
}

#[tokio::test]
async fn sync_reads_every_page_of_the_list() {
    let mut fake = FakeMcp::new(Era::Modern, server_tools());
    fake.page = 1;
    let server = serve(fake).await;
    let w = world(server.port, &format!("127.0.0.1:{}", server.port), false);
    sync(&w).await.unwrap();
    let text = read(&w);
    assert!(
        text.contains("get-forecast") && text.contains("put-city"),
        "{text}"
    );
    assert!(
        server.mcp.count("tools/list") >= 3,
        "도구 셋을 한 쪽에 하나씩 받아요"
    );
}

#[tokio::test]
async fn a_failed_sync_leaves_the_lock_exactly_as_it_was() {
    let (server, w) = authed(Era::Modern).await;
    sync(&w).await.unwrap();
    let baseline = read(&w);
    let inode = std::fs::metadata(&w.lock).unwrap().ino();
    let broken: Vec<(&str, Vec<McpTool>)> = vec![
        // 선언한 도구가 서버에 없다
        ("없는 도구", vec![forecast()]),
        // 설명에 자격이 섞였다
        (
            "설명의 자격",
            vec![
                McpTool::new("get-forecast", &format!("예보 {TOKEN}"), schema()),
                put_city(),
            ],
        ),
        // 스키마에 자격이 섞였다
        (
            "스키마의 자격",
            vec![
                McpTool::new(
                    "get-forecast",
                    "예보",
                    json!({"type": "object", "properties": {"city": {"type": "string", "enum": [TOKEN]}}}),
                ),
                put_city(),
            ],
        ),
        // 프로필 어휘 밖의 스키마
        (
            "어휘 밖",
            vec![
                McpTool::new(
                    "get-forecast",
                    "예보",
                    json!({"type": "object", "$ref": "#/x"}),
                ),
                put_city(),
            ],
        ),
        // 이름이 같은 도구가 둘
        ("이름 중복", vec![forecast(), forecast(), put_city()]),
    ];
    for (label, tools) in broken {
        server.mcp.set_tools(tools);
        let error = sync(&w).await.expect_err(label);
        assert!(
            !error.contains(TOKEN),
            "{label}: 오류에도 자격을 싣지 않아요\n{error}"
        );
        assert_eq!(read(&w), baseline, "{label}");
        assert_eq!(std::fs::metadata(&w.lock).unwrap().ino(), inode, "{label}");
    }
}

#[tokio::test]
async fn sync_connects_nowhere_when_the_profile_or_the_egress_cannot_be_fixed() {
    let server = serve(FakeMcp::new(Era::Modern, server_tools())).await;
    let w = world(server.port, &format!("127.0.0.1:{}", server.port), false);
    // 프로필 루트 밖의 폴더: 비밀을 읽을 프로필을 정할 수 없다
    let outside = w.tmp.path().join("elsewhere/weather");
    std::fs::create_dir_all(&outside).unwrap();
    for file in ["SKILL.md", "tools.yaml"] {
        std::fs::copy(w.skill.join(file), outside.join(file)).unwrap();
    }
    let error = mcp_sync::run_with(&w.settings, &outside, || panic!("보관소를 만들면 안 돼요"))
        .await
        .unwrap_err();
    assert!(error.contains("접속하지 않아요"), "{error}");
    // 선언한 egress가 서버 주소와 다르다
    let narrow = world(server.port, "127.0.0.1:1", false);
    let error = sync(&narrow).await.unwrap_err();
    assert!(error.contains("egress"), "{error}");
    assert!(!narrow.lock.exists());
    assert!(
        server.mcp.requests().is_empty(),
        "어느 경우에도 서버에 닿지 않았어요"
    );
}

#[tokio::test]
async fn sync_through_a_redirect_writes_nothing_and_follows_nothing() {
    let target = Counter::new().await;
    let mut fake = FakeMcp::new(Era::Modern, server_tools());
    fake.redirect = Some((
        "tools/list".into(),
        format!("http://127.0.0.1:{}/x", target.port),
    ));
    let server = serve(fake).await;
    let w = world(server.port, &format!("127.0.0.1:{}", server.port), false);
    let error = sync(&w).await.unwrap_err();
    assert!(error.contains("302"), "{error}");
    assert!(!w.lock.exists());
    assert_eq!(target.accepted().await, 0);
}
