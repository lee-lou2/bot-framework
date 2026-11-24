//! `serve`가 접속 출처(피어 주소)를 요청에 실어 준다: 인증 실패 한도가 출처마다 따로 세려면 필요하다.
//! (시험 서버 보조는 `X-Test-Peer`로 출처를 꾸며 넣으므로 실제 `serve`의 연결 정보는 여기서 따로 확인한다.)

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
    let listener = bot_admin::bind("127.0.0.1:0".parse().expect("주소"), false)
        .await
        .expect("바인드");
    let addr = listener.local_addr().expect("주소");
    tokio::spawn(async move {
        let _ = bot_admin::serve(listener, app, std::future::pending()).await;
    });
    let mut s = TcpStream::connect(addr).await.expect("연결");
    s.write_all(
        format!("GET /peer HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .expect("쓰기");
    let mut out = String::new();
    s.read_to_string(&mut out).await.expect("읽기");
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.ends_with("127.0.0.1"), "피어 주소가 실려야 한다: {out}");
}

mod support;
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

/// 시험 보조(`X-Test-Peer`)를 거치지 않은 운영과 같은 조립: `router` + `serve`만이다.
async fn production_server() -> SocketAddr {
    let fake = Arc::new(Fake::demo());
    let listener = bot_admin::bind("127.0.0.1:0".parse().expect("주소"), false)
        .await
        .expect("바인드");
    let addr = listener.local_addr().expect("주소");
    let deps = AdminDeps {
        read: fake.clone(),
        act: fake.clone(),
        broker: fake,
        auth: Arc::new(StaticTokens::new().with("operator-secret-1", viewer("local:owner", true))),
        profiles: Arc::new(demo_catalog()),
    };
    let app = router(AdminConfig::new(addr), deps).expect("라우터");
    tokio::spawn(async move {
        let _ = bot_admin::serve(listener, app, std::future::pending()).await;
    });
    addr
}

/// 출처를 속이려는 머리글을 있는 대로 붙인 요청. `n`마다 다른 «출처»를 주장한다.
async fn forged(addr: SocketAddr, token: &str, n: u8) -> support::Resp {
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
    support::raw(addr, "GET", "/v1/admin/me", None, &headers, None).await
}

#[tokio::test]
async fn forged_peer_headers_never_change_the_source_the_production_router_counts() {
    let addr = production_server().await;
    // 실패마다 다른 출처를 주장해도 실제 출처(이 연결)로 센다.
    for n in 1..=5 {
        assert_eq!(forged(addr, "wrong", n).await.status, 401);
    }
    // 올바른 비밀이어도, 또 다른 출처를 주장해도 실제 출처가 막혀 있다.
    for n in [6, 7, 127] {
        let r = forged(addr, "operator-secret-1", n).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (429, Some("rate_limited")),
            "{n}"
        );
    }
}

/// 운영 소스는 연결의 실제 피어 주소만 읽는다: 출처를 덮어쓸 수 있는 머리글 이름이 소스에 있으면 안 된다.
#[test]
fn production_sources_never_mention_headers_that_could_override_the_peer() {
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("폴더").filter_map(Result::ok) {
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
    assert!(files.len() > 10, "소스를 찾지 못했다");
    for file in files {
        let text = std::fs::read_to_string(&file)
            .expect("읽기")
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
