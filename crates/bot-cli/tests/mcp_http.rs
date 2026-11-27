//! Remote Streamable HTTP MCP tests over real sockets and `ReqwestWire`: protocol negotiation, JSON/SSE responses,
//! bearer isolation, redirect refusal, pre-connect egress checks, timeouts without retries, and `bot mcp sync`.
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
    McpTool::new("get-forecast", "City forecast", schema())
}

fn put_city() -> McpTool {
    McpTool::new("put-city", "Register a city", schema())
}

/// Server exposes three tools while the skill declares only two.
fn server_tools() -> Vec<McpTool> {
    vec![
        put_city(),
        McpTool::new("internal-admin", "Admin only", json!({"type": "object"})),
        forecast(),
    ]
}

fn wire() -> Arc<ReqwestWire> {
    Arc::new(ReqwestWire::new(Arc::new(SystemResolver)).expect("build HTTPS client"))
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
            assert_eq!(found["description"], "City forecast", "{at}");
            let result = call(&mut http)
                .await
                .unwrap_or_else(|e| panic!("{at}: {e:?}"));
            assert!(
                result.to_string().contains("ok:get-forecast"),
                "{at}: {result}"
            );
            http.close().await;
            // Exactly one tool call; only the legacy protocol sends DELETE to close the session.
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
            "{era:?}: credential leaked into tool result"
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
                "{era:?}: credential appeared in request body"
            );
            assert!(
                !request.path.contains(TOKEN),
                "{era:?}: credential appeared in request path"
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
                "{era:?}: credential appeared in non-authorization headers: {elsewhere:?}"
            );
        }
        // Wrong or missing credentials are rejected before invoking any tool.
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
            "{rpc}: must not resend redirected request"
        );
    }
    assert_eq!(
        target.accepted().await,
        0,
        "must never connect to redirect target"
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
        timeout: None,
    };
    // Plain HTTP is refused for non-loopback literals and hostnames even when allow_private is true.
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
    // HTTPS refuses loopback, private, and link-local literals unless allow_private is enabled.
    for host in ["127.0.0.1", "10.0.0.5", "169.254.169.254", "::1"] {
        let result = wire.send(request(true, host, false)).await;
        assert!(matches!(result, Err(WireFail::Unreached)), "https {host}");
    }
    assert_eq!(
        target.accepted().await,
        0,
        "refused requests must make zero network connections"
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
        "must not retry after timeout"
    );
}

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
    std::fs::write(skill.join("SKILL.md"), "Forecast skill\n").unwrap();
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
            "undeclared tools must not be written\n{first}"
        );
        let hash = definition_sha256("get-forecast", "City forecast", &schema()).unwrap();
        assert!(first.contains(&hash), "{first}");
        assert!(!first.contains(TOKEN), "credential must not enter lockfile");
        // Identical server state produces identical bytes and skips rewriting the lockfile.
        let inode = std::fs::metadata(&w.lock).unwrap().ino();
        sync(&w).await.unwrap();
        assert_eq!(read(&w), first);
        assert_eq!(
            std::fs::metadata(&w.lock).unwrap().ino(),
            inode,
            "{era:?}: unchanged lockfile must not be rewritten"
        );
        // Tool ordering from the server must not affect lockfile bytes.
        server
            .mcp
            .set_tools(server_tools().into_iter().rev().collect());
        sync(&w).await.unwrap();
        assert_eq!(read(&w), first);
        // Changing a description changes the hash and lockfile.
        let described = McpTool::new("get-forecast", "Different description", schema());
        server.mcp.set_tools(vec![described, put_city()]);
        sync(&w).await.unwrap();
        assert_ne!(read(&w), first);
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
        "expected paginated tools/list fetches across all three tools"
    );
}

#[tokio::test]
async fn a_failed_sync_leaves_the_lock_exactly_as_it_was() {
    let (server, w) = authed(Era::Modern).await;
    sync(&w).await.unwrap();
    let baseline = read(&w);
    let inode = std::fs::metadata(&w.lock).unwrap().ino();
    let broken: Vec<(&str, Vec<McpTool>)> = vec![
        ("missing tool", vec![forecast()]),
        (
            "credential in description",
            vec![
                McpTool::new("get-forecast", &format!("Forecast {TOKEN}"), schema()),
                put_city(),
            ],
        ),
        (
            "credential in schema",
            vec![
                McpTool::new(
                    "get-forecast",
                    "Forecast",
                    json!({"type": "object", "properties": {"city": {"type": "string", "enum": [TOKEN]}}}),
                ),
                put_city(),
            ],
        ),
        (
            "out-of-vocabulary schema",
            vec![
                McpTool::new(
                    "get-forecast",
                    "Forecast",
                    json!({"type": "object", "$ref": "#/x"}),
                ),
                put_city(),
            ],
        ),
        ("duplicate name", vec![forecast(), forecast(), put_city()]),
    ];
    for (label, tools) in broken {
        server.mcp.set_tools(tools);
        let error = sync(&w).await.expect_err(label);
        assert!(
            !error.contains(TOKEN),
            "{label}: error must not echo credentials\n{error}"
        );
        assert_eq!(read(&w), baseline, "{label}");
        assert_eq!(std::fs::metadata(&w.lock).unwrap().ino(), inode, "{label}");
    }
}

#[tokio::test]
async fn sync_connects_nowhere_when_the_profile_or_the_egress_cannot_be_fixed() {
    let server = serve(FakeMcp::new(Era::Modern, server_tools())).await;
    let w = world(server.port, &format!("127.0.0.1:{}", server.port), false);
    // Skill directory outside BOT_PROFILES cannot resolve a secret namespace and must fail before opening secrets.
    let outside = w.tmp.path().join("elsewhere/weather");
    std::fs::create_dir_all(&outside).unwrap();
    for file in ["SKILL.md", "tools.yaml"] {
        std::fs::copy(w.skill.join(file), outside.join(file)).unwrap();
    }
    let error = mcp_sync::run_with(&w.settings, &outside, || {
        panic!("must not open secret store")
    })
    .await
    .unwrap_err();
    assert!(error.contains("[not-a-skill]"), "{error}");
    // Declared egress does not match server host:port.
    let narrow = world(server.port, "127.0.0.1:1", false);
    let error = sync(&narrow).await.unwrap_err();
    assert!(error.contains("egress"), "{error}");
    assert!(!narrow.lock.exists());
    assert!(
        server.mcp.requests().is_empty(),
        "neither failure case may contact the server"
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
