//! Production `serve` peer address propagation and resistance to forged client-IP headers.

use axum::{Router, extract::ConnectInfo, routing::get};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[tokio::test]
async fn serve_hands_the_real_peer_address_to_every_request() {
    let app = Router::new().route(
        "/peer",
        get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.ip().to_string() }),
    );
    let listener = bot_admin::bind("127.0.0.1:0".parse().expect("addr"), false)
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = bot_admin::serve(listener, app, std::future::pending()).await;
    });
    let mut s = TcpStream::connect(addr).await.expect("connect");
    s.write_all(
        format!("GET /peer HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .expect("write");
    let mut out = String::new();
    s.read_to_string(&mut out).await.expect("read");
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(
        out.ends_with("127.0.0.1"),
        "real peer address must be attached: {out}"
    );
}

use bot_admin::{
    AdminConfig, AdminDeps, StaticTokens,
    auth::viewer,
    fake::{Fake, demo_catalog},
    router,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Production wiring (`router` + `serve` only, without test `X-Test-Peer` middleware).
async fn production_server() -> SocketAddr {
    let fake = Arc::new(Fake::demo());
    let listener = bot_admin::bind("127.0.0.1:0".parse().expect("addr"), false)
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let deps = AdminDeps {
        read: fake.clone(),
        act: fake.clone(),
        broker: fake,
        auth: Arc::new(StaticTokens::new().with("operator-secret-1", viewer("local:owner", true))),
        profiles: Arc::new(demo_catalog()),
    };
    let app = router(AdminConfig::new(addr), deps).expect("router");
    tokio::spawn(async move {
        let _ = bot_admin::serve(listener, app, std::future::pending()).await;
    });
    addr
}

async fn forged(addr: SocketAddr, token: &str, n: u8) -> crate::support::Resp {
    let auth = format!("Bearer {token}");
    let claims = [
        ("X-Test-Peer", format!("203.0.113.{n}")),
        ("X-Forwarded-For", format!("198.51.100.{n}")),
        ("Forwarded", format!("for=192.0.2.{n}")),
        ("X-Real-IP", format!("192.0.2.{n}")),
        ("True-Client-IP", format!("192.0.2.{n}")),
        ("CF-Connecting-IP", format!("192.0.2.{n}")),
    ];
    let mut headers: Vec<(&str, &str)> = vec![("Authorization", auth.as_str())];
    headers.extend(claims.iter().map(|(k, v)| (*k, v.as_str())));
    crate::support::raw(addr, "GET", "/v1/admin/me", None, &headers, None).await
}

#[tokio::test]
async fn forged_peer_headers_never_change_the_source_the_production_router_counts() {
    let addr = production_server().await;
    for n in 1..=5 {
        assert_eq!(forged(addr, "wrong", n).await.status, 401);
    }
    for n in [6, 7, 127] {
        let r = forged(addr, "operator-secret-1", n).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (429, Some("rate_limited")),
            "{n}"
        );
    }
}

#[test]
fn production_sources_never_mention_headers_that_could_override_the_peer() {
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(files.len() > 10, "expected source files under src/");
    for file in files {
        let text = std::fs::read_to_string(&file)
            .expect("read file")
            .to_ascii_lowercase();
        for banned in [
            "x-test-peer",
            "x-forwarded",
            "\"forwarded\"",
            "x-real-ip",
            "true-client-ip",
            "cf-connecting-ip",
            "x-client-ip",
        ] {
            assert!(!text.contains(banned), "{}: `{banned}`", file.display());
        }
    }
}
