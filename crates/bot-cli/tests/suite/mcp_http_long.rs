//! Per-request timeout overrides on `ReqwestWire`: explicit `WireRequest::timeout` overrides the
//! default idle read timeout while preserving pre-connect validation and connect timeouts.

use crate::mcp_http_support::closed_port;
use bot_broker::{
    mcp::wire::{RemoteWire, WireFail, WireMethod, WireRequest},
    proxy::SystemResolver,
};
use bot_cli::serve::remote::ReqwestWire;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

const DEFAULT_READ: Duration = Duration::from_millis(150);
const BODY: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[]}}"#;

/// Loopback server that waits `delay` after reading `{}` before replying (or hangs if `None`).
async fn slow_server(delay: Option<Duration>) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut seen = Vec::new();
                while !seen.ends_with(b"{}") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                let Some(delay) = delay else {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                };
                tokio::time::sleep(delay).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
                    BODY.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (port, task)
}

fn wire() -> ReqwestWire {
    ReqwestWire::with_read_timeout(Arc::new(SystemResolver), DEFAULT_READ).unwrap()
}

fn request(port: u16, timeout: Option<Duration>) -> WireRequest {
    WireRequest {
        method: WireMethod::Post,
        tls: false,
        host: "127.0.0.1".into(),
        port,
        path: "/mcp".into(),
        headers: vec![("Content-Type".into(), "application/json".into(), false)],
        body: b"{}".to_vec(),
        allow_private: false,
        timeout,
    }
}

#[tokio::test]
async fn a_request_with_its_own_limit_outlives_the_default_read_limit_and_one_without_does_not() {
    let (port, task) = slow_server(Some(Duration::from_millis(700))).await;
    let wire = wire();
    assert!(matches!(
        wire.send(request(port, None)).await,
        Err(WireFail::Lost)
    ));
    let response = wire
        .send(request(port, Some(Duration::from_secs(5))))
        .await
        .expect("reply within per-request timeout must succeed");
    assert_eq!(response.status, 200);
    let mut body = response.body;
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await.unwrap() {
        bytes.extend(chunk);
    }
    assert_eq!(String::from_utf8(bytes).unwrap(), BODY);
    task.abort();
}

#[tokio::test]
async fn the_requests_own_limit_still_ends_a_server_that_never_answers() {
    let (port, task) = slow_server(None).await;
    let started = std::time::Instant::now();
    let result = wire()
        .send(request(port, Some(Duration::from_millis(400))))
        .await;
    assert!(
        matches!(result, Err(WireFail::Lost)),
        "per-request timeout must abort"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "{:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    task.abort();
}

#[tokio::test]
async fn a_long_limit_does_not_loosen_the_checks_before_the_connection() {
    // Connection refusal fails before request delivery (`Unreached`) regardless of long timeout.
    let dead = closed_port().await;
    let started = std::time::Instant::now();
    let result = wire()
        .send(request(dead, Some(Duration::from_secs(900))))
        .await;
    assert!(matches!(result, Err(WireFail::Unreached)));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let mut private = request(dead, Some(Duration::from_secs(900)));
    private.tls = true;
    private.host = "10.0.0.5".into();
    assert!(matches!(
        wire().send(private).await,
        Err(WireFail::Unreached)
    ));
}
