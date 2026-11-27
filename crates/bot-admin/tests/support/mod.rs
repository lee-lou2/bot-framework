//! Test harness: serves the fake store on a real socket and sends raw HTTP/1.1 requests.
#![allow(dead_code)]

pub mod collector;
pub mod holdfx;

use axum::{
    extract::{ConnectInfo, Request},
    middleware::{Next, from_fn},
};
use bot_admin::{AdminConfig, AdminDeps, StaticTokens, auth::viewer, fake::Fake, router};
use serde_json::Value;
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub const OPERATOR: &str = "operator-secret-1";
pub const GUEST: &str = "guest-secret-2";

pub struct Server {
    pub addr: SocketAddr,
    pub fake: Arc<Fake>,
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub async fn start(fake: Fake) -> Server {
    start_with(fake, None).await
}

pub async fn start_with(fake: Fake, traces: Option<Arc<bot_admin::Exporter>>) -> Server {
    start_cfg(fake, |mut cfg| {
        cfg.traces = traces;
        cfg
    })
    .await
}

pub async fn start_cfg(fake: Fake, tweak: impl FnOnce(AdminConfig) -> AdminConfig) -> Server {
    let fake = Arc::new(fake);
    let listener = bot_admin::bind("127.0.0.1:0".parse().expect("addr"), false)
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let auth = StaticTokens::new()
        .with(OPERATOR, viewer("local:owner", true))
        .with(GUEST, viewer("local:guest", false));
    let deps = AdminDeps {
        read: fake.clone(),
        act: fake.clone(),
        broker: fake.clone(),
        auth: Arc::new(auth),
        profiles: Arc::new(bot_admin::fake::demo_catalog()),
    };
    let app = router(tweak(AdminConfig::new(addr)), deps).expect("router");
    // Outermost test-only layer overrides ConnectInfo when `X-Test-Peer` is set.
    let app = app.layer(from_fn(|mut req: Request, next: Next| async move {
        let peer = req
            .headers()
            .get("x-test-peer")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<IpAddr>().ok());
        if let Some(ip) = peer {
            req.extensions_mut()
                .insert(ConnectInfo(SocketAddr::new(ip, 40000)));
        }
        next.run(req).await
    }));
    tokio::spawn(async move {
        let _ = bot_admin::serve(listener, app, std::future::pending()).await;
    });
    Server { addr, fake }
}

/// Sends a raw HTTP/1.1 request; `host: None` uses `addr`.
pub async fn raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Resp {
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        host.map_or_else(|| addr.to_string(), str::to_owned)
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    req.push_str(body.unwrap_or_default());
    let mut s = TcpStream::connect(addr).await.expect("connect");
    s.write_all(req.as_bytes()).await.expect("write");
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.expect("read");
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status line");
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    Resp {
        status,
        headers,
        body: body.to_owned(),
    }
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

pub async fn get(s: &Server, token: &str, path: &str) -> Resp {
    raw(
        s.addr,
        "GET",
        path,
        None,
        &[("Authorization", &bearer(token))],
        None,
    )
    .await
}

pub async fn csrf(s: &Server, token: &str) -> String {
    let me = get(s, token, "/v1/admin/me").await.json();
    me["csrf"].as_str().expect("csrf").to_owned()
}

pub async fn post(s: &Server, token: &str, path: &str, body: &str) -> Resp {
    let csrf = csrf(s, token).await;
    raw(
        s.addr,
        "POST",
        path,
        None,
        &[
            ("Authorization", &bearer(token)),
            ("Content-Type", "application/json"),
            ("X-Bot-Csrf", &csrf),
        ],
        Some(body),
    )
    .await
}
