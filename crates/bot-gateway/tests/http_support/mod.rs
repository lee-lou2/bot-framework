//! Shared HTTP test harness: live TCP server and raw HTTP/1.1 client.
#![allow(dead_code)]
use bot_core::{
    id::{BindingId, PrincipalId, ProfileId, RequestId},
    intake::RequestKey,
    profile::Adapter,
};
use bot_gateway::{
    active::ActiveCheck,
    host::HostConfig,
    http::{HttpGateway, token::sign},
    ingress::Ingress,
    testing::*,
};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

pub const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
pub const NOW: u64 = 1_800_000_000;
pub const HOST: &str = r#"{
    "http": {"endpoints": {"inbox": {"issuer": "https://issuer.example", "audience": "bot",
        "key": "inbox-key", "sync_wait_ms": 1500}}},
    "limits": {"per_principal_per_min": 20, "body_bytes": 1024}
}"#;

pub struct Rig {
    pub addr: SocketAddr,
    pub store: Arc<MemoryStore>,
    pub catalog: Arc<TestCatalog>,
    pub stop: CancellationToken,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub fn fixture() -> Fixture {
    let notify = http_trigger(
        "notify",
        "inbox",
        "POST",
        "/notify",
        tool_target("notify.send", "{}"),
    );
    let mut agenda = sync(http_trigger(
        "agenda",
        "inbox",
        "POST",
        "/agenda",
        tool_target("agenda.delete", "{}"),
    ));
    agenda.input.insert("event".into(), "body.event".into());
    profile("personal")
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .member("http:inbox/svc", &["svc"])
        .member("http:inbox/other", &["svc"])
        .role("svc", &["calendar.delete"])
        .policy_scopes(&["calendar.delete"])
        .trigger("main", notify)
        .trigger("main", agenda)
}

pub async fn rig() -> Rig {
    rig_with(|_| {}).await
}

pub async fn rig_with(tune: impl FnOnce(&mut HostConfig)) -> Rig {
    rig_full(
        vec![fixture()],
        MemorySecrets::new(&[("personal", "inbox-key", KEY)]),
        tune,
    )
    .await
}

pub async fn rig_full(
    profiles: Vec<Fixture>,
    secrets: Arc<MemorySecrets>,
    tune: impl FnOnce(&mut HostConfig),
) -> Rig {
    rig_active(profiles, secrets, None, tune).await
}

pub async fn rig_active(
    profiles: Vec<Fixture>,
    secrets: Arc<MemorySecrets>,
    active: Option<Arc<dyn ActiveCheck>>,
    tune: impl FnOnce(&mut HostConfig),
) -> Rig {
    let store = MemoryStore::new();
    let mut host = HostConfig::from_json(HOST).unwrap();
    tune(&mut host);
    let catalog = TestCatalog::new(profiles.into_iter().map(Fixture::build));
    let mut ingress = Ingress::new(
        catalog.clone(),
        Arc::new(host),
        store.clone(),
        MemoryPrivate::new(),
    );
    if let Some(active) = active {
        ingress = ingress.with_active(active);
    }
    let gateway = HttpGateway::new(Arc::new(ingress), store.clone(), secrets).with_clock(|| NOW);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    tokio::spawn(Arc::new(gateway).serve(listener, stop.clone()));
    Rig {
        addr,
        store,
        catalog,
        stop,
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

pub async fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(text.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut raw))
        .await
        .expect("response timeout")
        .unwrap();
    let raw = String::from_utf8(raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
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
    Reply {
        status,
        headers,
        body: body.to_string(),
    }
}

pub fn token_for(sub: &str) -> String {
    sign(
        &json!({"iss": "https://issuer.example", "aud": "bot", "sub": sub, "exp": NOW + 3600}),
        KEY,
    )
}

pub async fn post(rig: &Rig, path: &str, sub: &str, key: &str, body: Value) -> Reply {
    let auth = format!("Bearer {}", token_for(sub));
    send(
        rig.addr,
        "POST",
        path,
        &[
            ("Authorization", &auth),
            ("Idempotency-Key", key),
            ("Content-Type", "application/json"),
        ],
        &body.to_string(),
    )
    .await
}

pub async fn get(rig: &Rig, path: &str, sub: &str) -> Reply {
    let auth = format!("Bearer {}", token_for(sub));
    send(rig.addr, "GET", path, &[("Authorization", &auth)], "").await
}

pub fn key_of(sub: &str, request: &str) -> RequestKey {
    RequestKey {
        profile: ProfileId("personal".into()),
        binding: BindingId("inbox".into()),
        principal: PrincipalId(format!("http:inbox/{sub}")),
        request: RequestId(request.into()),
    }
}
