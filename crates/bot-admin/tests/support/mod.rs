//! 시험 보조: 가짜 저장소를 실제 소켓에 올리고 날것의 HTTP/1.1로 요청한다(Host 검사까지 실제 경로로 시험한다).
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
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("JSON 아님({e}): {}", self.body))
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

/// 추적 내보내기를 붙여 시작한다.
pub async fn start_with(fake: Fake, traces: Option<Arc<bot_admin::Exporter>>) -> Server {
    start_cfg(fake, |mut cfg| {
        cfg.traces = traces;
        cfg
    })
    .await
}

/// 설정을 고쳐서 시작한다(훅·시간 제한 같은 선택 입력을 시험할 때).
pub async fn start_cfg(fake: Fake, tweak: impl FnOnce(AdminConfig) -> AdminConfig) -> Server {
    let fake = Arc::new(fake);
    let listener = bot_admin::bind("127.0.0.1:0".parse().expect("주소"), false)
        .await
        .expect("바인드");
    let addr = listener.local_addr().expect("주소");
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
    let app = router(tweak(AdminConfig::new(addr)), deps).expect("라우터");
    // 시험용 출처 흉내: `X-Test-Peer: <IP>`가 있으면 그 주소에서 온 것으로 본다(가장 바깥 층이라 인증보다 먼저 돈다).
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

/// `host`를 `None`으로 두면 서버의 실제 주소를 쓴다.
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
    let mut s = TcpStream::connect(addr).await.expect("연결");
    s.write_all(req.as_bytes()).await.expect("쓰기");
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.expect("읽기");
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())
        .expect("상태 줄");
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

/// 인증된 GET으로 CSRF 토큰을 받는다.
pub async fn csrf(s: &Server, token: &str) -> String {
    let me = get(s, token, "/v1/admin/me").await.json();
    me["csrf"].as_str().expect("csrf").to_owned()
}

/// 올바른 CSRF 토큰·JSON 헤더를 붙여 POST한다.
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
