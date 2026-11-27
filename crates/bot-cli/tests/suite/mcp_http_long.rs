//! 긴 호출은 전송의 기본 무응답 읽기 제한에 먼저 끊기지 않는다: 요청이 자기 제한(`WireRequest::timeout`)을 실으면 운영 전송(`ReqwestWire`)은 그 제한을 쓰고,
//! 싣지 않은 요청은 기본 제한을 그대로 쓴다. 연결을 맺기 전의 제한은 두 경우 모두 유지된다. 서버는 루프백의 임시 소켓뿐이다(외부 서비스·실제 키 없음).
//! 기본 제한은 시험에서 짧게(150ms) 줄인 전송으로 본다 — 운영 값(120초)을 기다리지 않는다.

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

/// 요청을 읽은 뒤 `delay` 동안 아무것도 보내지 않다가 JSON 응답 하나를 주는 서버(`delay`가 `None`이면 끝내 답하지 않는다).
async fn slow_server(delay: Option<Duration>) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                // 요청 머리와 본문(`{}`)을 읽는다
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
    // 기본 제한(150ms)만 있는 요청은 700ms 뒤에야 오는 답을 기다리지 못한다: 서버가 받았는지 모르는 `Lost`다(제한이 없었다면 답을 받았을 것이다)
    assert!(matches!(
        wire.send(request(port, None)).await,
        Err(WireFail::Lost)
    ));
    // 자기 제한(5초)을 실은 요청은 같은 서버의 늦은 답을 받는다
    let response = wire
        .send(request(port, Some(Duration::from_secs(5))))
        .await
        .expect("자기 제한 안에 오는 답은 받는다");
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
    assert!(matches!(result, Err(WireFail::Lost)), "자기 제한이 끝낸다");
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "{:?}",
        started.elapsed()
    );
    // 서버는 60초를 버틴다: 요청 자신의 제한이 없었다면 여기까지 오지 못한다(부하가 큰 환경에서도 흔들리지 않게 넉넉히 잡는다)
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    task.abort();
}

#[tokio::test]
async fn a_long_limit_does_not_loosen_the_checks_before_the_connection() {
    // 아무도 듣지 않는 포트: 연결을 맺기 전 실패이므로 긴 제한을 실어도 `Unreached`(요청이 서버에 닿지 않았다)다
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
    // 비공개 주소(https)는 긴 제한이 있어도 연결 전에 거절한다
    let mut private = request(dead, Some(Duration::from_secs(900)));
    private.tls = true;
    private.host = "10.0.0.5".into();
    assert!(matches!(
        wire().send(private).await,
        Err(WireFail::Unreached)
    ));
}
