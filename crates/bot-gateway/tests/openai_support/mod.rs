//! Shared OpenAI-compatible ingress test harness: live TCP server (`/v1`), in-memory `ApiRuns`, and raw HTTP/1.1 client.
#![allow(dead_code, unused_imports)]
mod handoffs;
mod secrets;
mod sse;
use bot_core::{
    api::ApiState,
    id::{PrincipalId, ProfileId},
    port::{Error, ProfileSecrets, Reply, SecretStore},
    profile::Adapter,
};
use bot_gateway::{
    active::ActiveCheck,
    host::HostConfig,
    http::{HttpGateway, Passed, Probe, token::sign},
    ingress::Ingress,
    testing::{
        api::{MemoryApi, MemoryBundles},
        ports::MemoryActive,
        *,
    },
};
pub use handoffs::Handoffs;
pub use secrets::FlakySecrets;
use serde_json::{Value, json};
pub use sse::{Next, Sse, data_json};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

pub const BUILD_KEY: &str = concat!("build-key-", "0123456789", "abcdef0123", "456789");
pub const BUILD_NEXT_KEY: &str = concat!("build-next-", "0123456789", "abcdef0123", "456789");
pub const OPS_KEY: &str = concat!("ops-key-", "0123456789", "abcdef0123", "456789ab");
/// HS256 key for the co-hosted `inbox` webhook endpoint.
pub const JWT_KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
pub const NOW: u64 = 1_800_000_000;

pub const HOST: &str = r#"{
    "http": {"endpoints": {
        "openai": {"kind": "openai", "wait_ms": 3000, "max_active_runs": 3, "keys": {
            "build": {"secret": "build-key", "subject": "build"},
            "build-next": {"secret": "build-key-next", "subject": "build"},
            "ops": {"secret": "ops-key", "subject": "ops"}}},
        "inbox": {"issuer": "https://issuer.example", "audience": "bot", "key": "inbox-key", "sync_wait_ms": 1500}}},
    "limits": {"per_principal_per_min": 100, "per_entrance_per_min": 1000}
}"#;

pub fn secrets() -> Arc<MemorySecrets> {
    MemorySecrets::new(&[
        ("personal", "build-key", BUILD_KEY.as_bytes()),
        ("personal", "build-key-next", BUILD_NEXT_KEY.as_bytes()),
        ("personal", "ops-key", OPS_KEY.as_bytes()),
        ("personal", "inbox-key", JWT_KEY),
    ])
}

/// Profile mutation parameters for revocation and reassignment tests.
#[derive(Clone, Copy)]
pub struct Shape {
    pub build_member: bool,
    pub assistant_agent: &'static str,
    pub assistant_binding: bool,
    /// When false, `build` receives only `calendar.read` instead of `run`.
    pub build_runs: bool,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            build_member: true,
            assistant_agent: "main",
            assistant_binding: true,
            build_runs: true,
        }
    }
}

pub fn fixture() -> Fixture {
    fixture_shaped(Shape::default())
}

pub fn fixture_shaped(shape: Shape) -> Fixture {
    let openai = |model: &str| Adapter::OpenAi {
        endpoint: "openai".into(),
        model: model.into(),
        approvers: BTreeMap::new(),
    };
    let mut fixture = profile("personal")
        .agent("side", &["run"])
        .agent_scopes("main", &["run"])
        .binding("helper", openai("helper"), "side")
        .member("http:openai/ops", &["viewer"])
        .role(
            "runner",
            if shape.build_runs {
                &["run"][..]
            } else {
                &["calendar.read"][..]
            },
        )
        .role("viewer", &["calendar.read"])
        .policy_scopes(&["run", "calendar.read"])
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .member("http:inbox/svc", &["runner"])
        .trigger(
            "main",
            http_trigger(
                "notify",
                "inbox",
                "POST",
                "/notify",
                tool_target("notify.send", "{}"),
            ),
        );
    if shape.assistant_binding {
        fixture = fixture.binding("assistant", openai("assistant"), shape.assistant_agent);
    }
    if shape.build_member {
        fixture = fixture.member("http:openai/build", &["runner"]);
    }
    fixture
}

pub struct Rig {
    pub addr: SocketAddr,
    /// IPv6 loopback address when bound dual-stack to simulate a second source IP.
    pub alt: Option<SocketAddr>,
    pub api: Arc<MemoryApi>,
    pub bundles: Arc<MemoryBundles>,
    pub store: Arc<MemoryStore>,
    pub secrets: Arc<FlakySecrets>,
    pub catalog: Arc<TestCatalog>,
    pub active: Arc<MemoryActive>,
    pub stop: CancellationToken,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub struct Setup {
    pub profiles: Vec<Fixture>,
    pub keep_alive: Option<Duration>,
    pub models_budget: Option<Duration>,
    pub probe: Option<Probe>,
    /// Auth failure budget `(max, window, block)`; defaults effectively unlimited for unrelated tests.
    pub auth_failures: (usize, Duration, Duration),
    pub connections: Option<usize>,
    /// Bind `[::]` so tests can connect from both IPv4 (`127.0.0.1`) and IPv6 (`::1`).
    pub dual_stack: bool,
    pub tune: Box<dyn FnOnce(&mut HostConfig)>,
    pub bundles: Option<i64>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            profiles: vec![fixture()],
            keep_alive: None,
            models_budget: None,
            probe: None,
            auth_failures: (1_000_000, Duration::from_secs(60), Duration::from_secs(60)),
            connections: None,
            dual_stack: false,
            tune: Box::new(|_| {}),
            bundles: Some(1_750_000_000_123),
        }
    }
}

pub async fn rig() -> Rig {
    rig_with(Setup::default()).await
}

pub async fn rig_with(setup: Setup) -> Rig {
    rig_host(HOST, setup).await
}

pub async fn rig_host(host_json: &str, setup: Setup) -> Rig {
    let api = MemoryApi::new();
    let bundles = MemoryBundles::new(setup.bundles);
    let store = MemoryStore::new();
    let secrets = FlakySecrets::new(secrets());
    let mut host = HostConfig::from_json(host_json).unwrap();
    (setup.tune)(&mut host);
    let catalog = TestCatalog::new(setup.profiles.into_iter().map(Fixture::build));
    let active = MemoryActive::new(Some("set-1"));
    let ingress = Ingress::new(
        catalog.clone(),
        Arc::new(host),
        store.clone(),
        MemoryPrivate::new(),
    )
    .with_active(active.check("set-1") as Arc<dyn ActiveCheck>);
    let mut gateway = HttpGateway::new(Arc::new(ingress), store.clone(), secrets.clone())
        .with_clock(|| NOW)
        .with_openai(api.clone(), bundles.clone());
    if let Some(every) = setup.keep_alive {
        gateway = gateway.with_keep_alive(every);
    }
    if let Some(budget) = setup.models_budget {
        gateway = gateway.with_models_budget(budget);
    }
    if let Some(probe) = setup.probe {
        gateway = gateway.with_probe(probe);
    }
    if let Some(limit) = setup.connections {
        gateway = gateway.with_connection_limit(limit);
    }
    let (max, window, block) = setup.auth_failures;
    gateway = gateway.with_auth_failure_limit(max, window, block);
    let listener = TcpListener::bind(if setup.dual_stack {
        "[::]:0"
    } else {
        "127.0.0.1:0"
    })
    .await
    .unwrap();
    let port = listener.local_addr().unwrap().port();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let alt = setup
        .dual_stack
        .then(|| SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)));
    let stop = CancellationToken::new();
    tokio::spawn(Arc::new(gateway).serve(listener, stop.clone()));
    Rig {
        addr,
        alt,
        api,
        bundles,
        store,
        secrets,
        catalog,
        active,
        stop,
    }
}

#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn headers_named(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// Extracts `(type, code, param)` from an OpenAI error JSON envelope.
    pub fn error(&self) -> (String, String, Value) {
        let error = &self.json()["error"];
        (
            error["type"].as_str().unwrap_or_default().to_owned(),
            error["code"].as_str().unwrap_or_default().to_owned(),
            error["param"].clone(),
        )
    }
}

fn head_of(raw: &str) -> (u16, Vec<(String, String)>, &str) {
    let (head, body) = raw.split_once("\r\n\r\n").expect("head and body");
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
    (status, headers, body)
}

fn dechunk(mut body: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).expect("chunk size");
        if size == 0 {
            break;
        }
        chunks.push(rest[..size].to_owned());
        body = &rest[size + 2..];
    }
    chunks
}

pub async fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Resp {
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(text.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut raw))
        .await
        .expect("response timeout")
        .unwrap();
    let raw = String::from_utf8(raw).unwrap();
    let (status, headers, body) = head_of(&raw);
    let chunked = headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
    let body = if chunked {
        dechunk(body).concat()
    } else {
        body.to_owned()
    };
    Resp {
        status,
        headers,
        body,
    }
}

pub fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

pub async fn chat(rig: &Rig, key: &str, idem: Option<&str>, body: &Value) -> Resp {
    chat_raw(rig, key, idem, &[], &body.to_string()).await
}

pub async fn chat_raw(
    rig: &Rig,
    key: &str,
    idem: Option<&str>,
    extra: &[(&str, &str)],
    body: &str,
) -> Resp {
    let auth = bearer(key);
    let mut headers = vec![
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];
    if let Some(idem) = idem {
        headers.push(("Idempotency-Key", idem));
    }
    headers.extend_from_slice(extra);
    send(rig.addr, "POST", "/v1/chat/completions", &headers, body).await
}

pub async fn get(rig: &Rig, key: &str, path: &str) -> Resp {
    let auth = bearer(key);
    send(rig.addr, "GET", path, &[("Authorization", &auth)], "").await
}

pub fn ask(model: &str, text: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": text}]})
}

pub fn ask_stream(model: &str, text: &str, usage: bool) -> Value {
    let mut body = ask(model, text);
    body["stream"] = json!(true);
    if usage {
        body["stream_options"] = json!({"include_usage": true});
    }
    body
}

pub fn answer(text: &str) -> ApiState {
    ApiState::Final {
        text: text.to_owned(),
        truncated: false,
        usage: None,
    }
}

pub fn principal(subject: &str) -> PrincipalId {
    PrincipalId(format!("http:openai/{subject}"))
}

pub fn jwt(sub: &str) -> String {
    sign(
        &json!({"iss": "https://issuer.example", "aud": "bot", "sub": sub, "exp": NOW + 3600}),
        JWT_KEY,
    )
}

pub async fn send_raw(addr: SocketAddr, request: &[u8]) -> Resp {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut raw))
        .await
        .expect("response timeout")
        .unwrap();
    let raw = String::from_utf8(raw).unwrap();
    let (status, headers, body) = head_of(&raw);
    Resp {
        status,
        headers,
        body: body.to_owned(),
    }
}

pub fn finish_later(rig: &Rig, key: &str, state: ApiState, delay: Duration) {
    let (api, key) = (rig.api.clone(), key.to_owned());
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(delay).await;
            if api.run_of(&key).is_some() {
                api.set_state(&key, state);
                return;
            }
        }
    });
}

pub fn spawn_chat(
    rig: &Rig,
    key: &'static str,
    idem: Option<&str>,
    body: Value,
) -> tokio::task::JoinHandle<Resp> {
    let addr = rig.addr;
    let idem = idem.map(str::to_owned);
    tokio::spawn(async move {
        let auth = bearer(key);
        let mut headers = vec![
            ("Authorization", auth.as_str()),
            ("Content-Type", "application/json"),
        ];
        if let Some(idem) = idem.as_deref() {
            headers.push(("Idempotency-Key", idem));
        }
        send(
            addr,
            "POST",
            "/v1/chat/completions",
            &headers,
            &body.to_string(),
        )
        .await
    })
}

pub const PENDING: &str =
    "Awaiting approval. Query again with the same Idempotency-Key and request.";
pub const RUNNING: &str =
    "Request is still in progress. Query again with the same Idempotency-Key and request.";

pub fn tuned(wait_ms: u64, max_active: u32) -> Setup {
    Setup {
        tune: Box::new(move |host| {
            let endpoint = host.http.openai.get_mut("openai").unwrap();
            endpoint.wait_ms = Some(wait_ms);
            endpoint.max_active_runs = Some(max_active);
        }),
        ..Setup::default()
    }
}

pub fn content(reply: &Resp) -> String {
    reply.json()["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_else(|| panic!("{}", reply.body))
        .to_owned()
}
