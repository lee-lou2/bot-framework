//! OpenAI 호환 입구 시험 공용 조립: 실제 TCP 서버(루트 `/v1`), 메모리 `ApiRuns`, 원시 HTTP/1.1 클라이언트(JSON·SSE).
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
/// 웹훅 입구 `inbox`의 HS256 키(같은 호스트에서 JWT 웹훅이 그대로인지 보는 데 쓴다)
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

/// 프로필의 모양을 바꿔 보는 값(권한 철회·재지정 시험)
#[derive(Clone, Copy)]
pub struct Shape {
    /// 멤버 `http:openai/build`가 있는가
    pub build_member: bool,
    /// 모델 `assistant`가 가리키는 에이전트
    pub assistant_agent: &'static str,
    /// 모델 `assistant`의 바인딩이 있는가
    pub assistant_binding: bool,
    /// 멤버 `build`의 역할이 `run` 스코프를 주는가(참이 아니면 읽기 스코프만 준다)
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

/// 프로필 `personal`: 모델 `assistant`(에이전트 main)와 `helper`(에이전트 side), 멤버 `build`(실행 가능)·`ops`(읽기만)
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
    /// 접속 주소(IPv4 루프백)
    pub addr: SocketAddr,
    /// 듀얼 스택일 때 다른 출처로 접속하는 IPv6 루프백 주소
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
    /// 키 인증 실패 예산(횟수, 집계 창, 차단 시간). 기본은 사실상 무제한이라 일반 시험이 영향받지 않는다.
    pub auth_failures: (usize, Duration, Duration),
    /// 동시 연결 한도(없으면 운영 기본값)
    pub connections: Option<usize>,
    /// 듀얼 스택(`[::]`)으로 열어 IPv4(`127.0.0.1`)와 IPv6(`::1`) 두 출처로 접속할 수 있게 한다
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

    /// `{"error":{...}}`의 (type, code, param)
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
    let (head, body) = raw.split_once("\r\n\r\n").expect("머리와 본문");
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

/// 청크 전송 본문을 풀어 합친다(청크 하나가 SSE 이벤트 하나)
fn dechunk(mut body: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).expect("청크 크기");
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
        .expect("응답 시간 초과")
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

/// `POST /v1/chat/completions` (인증·Content-Type 포함). `idem`이 None이면 키 헤더를 보내지 않는다.
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

/// 웹훅 입구용 JWT(같은 호스트에서 웹훅이 그대로인지 보는 데 쓴다)
pub fn jwt(sub: &str) -> String {
    sign(
        &json!({"iss": "https://issuer.example", "aud": "bot", "sub": sub, "exp": NOW + 3600}),
        JWT_KEY,
    )
}

/// 요청 바이트를 그대로 보내고 응답을 읽는다(청크 전송·느린 본문·헤더 조작용)
pub async fn send_raw(addr: SocketAddr, request: &[u8]) -> Resp {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut raw))
        .await
        .expect("응답 시간 초과")
        .unwrap();
    let raw = String::from_utf8(raw).unwrap();
    let (status, headers, body) = head_of(&raw);
    Resp {
        status,
        headers,
        body: body.to_owned(),
    }
}

/// `delay` 뒤에 이 멱등 키의 작업을 끝낸다(worker 완료 흉내)
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

/// 요청을 백그라운드로 보낸다(대기 중 상황을 바꾸는 시험용)
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
    "승인 대기 중입니다. 같은 Idempotency-Key와 같은 요청으로 다시 조회하세요.";
pub const RUNNING: &str =
    "작업을 처리 중입니다. 같은 Idempotency-Key와 같은 요청으로 다시 조회하세요.";

/// 입구 `openai`의 총 대기 시간(ms)과 실행 수 한도를 바꾼 조립
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

/// 성공 응답의 `choices[0].message.content`
pub fn content(reply: &Resp) -> String {
    reply.json()["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_else(|| panic!("{}", reply.body))
        .to_owned()
}
