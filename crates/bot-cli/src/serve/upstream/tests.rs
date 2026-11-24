use super::*;
use bot_broker::resolve::HostPort;
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// 연결마다 요청을 읽고 `reply`가 돌려준 바이트를 그대로 쓴다(빈 값이면 아무것도 쓰지 않고 닫는다). 받은 요청 글과 연결 수를 모은다.
struct Server {
    addr: SocketAddr,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    connections: Arc<AtomicUsize>,
}

async fn serve(reply: impl Fn(usize) -> Vec<u8> + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    let (log, count) = (seen.clone(), connections.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let n = count.fetch_add(1, Ordering::SeqCst);
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            // 머리 끝(빈 줄)과 본문(Content-Length)까지 읽는다
            while let Ok(read) = socket.read(&mut buf).await {
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..read]);
                let text = String::from_utf8_lossy(&raw).into_owned();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
            }
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&raw).into_owned());
            let out = reply(n);
            if !out.is_empty() {
                let _ = socket.write_all(&out).await;
            }
            let _ = socket.shutdown().await;
        }
    });
    Server {
        addr,
        seen,
        connections,
    }
}

fn http(status: &str, extra: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn request(server: &Server, headers: &[(&str, &str)], body: &str) -> UpstreamRequest {
    UpstreamRequest {
        endpoint: HostPort {
            host: "127.0.0.1".into(),
            port: server.addr.port(),
        },
        path: "/v1/messages".into(),
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        body: body.as_bytes().to_vec(),
    }
}

fn upstream() -> HttpsUpstream {
    HttpsUpstream::over_http(1024).unwrap()
}

#[tokio::test]
async fn a_request_goes_out_once_with_its_path_headers_and_body_and_the_answer_comes_back_whole() {
    let server = serve(|_| {
        http(
            "200 OK",
            "content-type: application/json\r\n",
            r#"{"ok":true}"#,
        )
    })
    .await;
    let got = upstream()
        .send(request(
            &server,
            &[
                ("x-api-key", "k-secret"),
                ("anthropic-version", "2023-06-01"),
            ],
            r#"{"model":"m"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        (got.status, got.body.as_slice()),
        (200, br#"{"ok":true}"#.as_slice())
    );
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let text = seen[0].to_ascii_lowercase();
    assert!(text.starts_with("post /v1/messages http/1.1"), "{text}");
    assert!(
        text.contains("x-api-key: k-secret") && text.contains("anthropic-version: 2023-06-01"),
        "{text}"
    );
    assert!(text.ends_with(r#"{"model":"m"}"#), "{text}");
    // 상류의 거절은 그대로 돌려준다(상태·본문)
    let server = serve(|_| {
        http(
            "429 Too Many Requests",
            "retry-after: 3\r\n",
            r#"{"error":"slow"}"#,
        )
    })
    .await;
    let got = upstream().send(request(&server, &[], "{}")).await.unwrap();
    assert_eq!(
        (got.status, got.body.as_slice()),
        (429, br#"{"error":"slow"}"#.as_slice())
    );
}

#[tokio::test]
async fn nothing_is_sent_twice_no_redirect_is_followed_and_a_lost_answer_is_unavailable() {
    // 리다이렉트는 따라가지 않는다: 다른 곳으로 보내지 않고 그 응답을 그대로 돌려준다
    let server = serve(|_| {
        http(
            "302 Found",
            "location: http://127.0.0.1:1/elsewhere\r\n",
            "",
        )
    })
    .await;
    let got = upstream()
        .send(request(&server, &[("x-api-key", "k")], "{}"))
        .await
        .unwrap();
    assert_eq!(got.status, 302);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    // 응답 없이 연결이 닫혀도 다시 보내지 않는다(요청은 정확히 한 번 나갔다)
    let server = serve(|_| Vec::new()).await;
    let error = upstream().send(request(&server, &[], "{}")).await.err();
    assert!(matches!(error, Some(Error::Unavailable)), "{error:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        1,
        "숨은 재전송이 없어요"
    );
    // 연결할 수 없는 곳도 Unavailable
    let dead = serve(|_| Vec::new()).await;
    let address = dead.addr;
    drop(dead);
    let mut gone = request(
        &Server {
            addr: address,
            seen: Default::default(),
            connections: Default::default(),
        },
        &[],
        "{}",
    );
    gone.endpoint.port = address.port();
    let error = upstream().send(gone).await.err();
    assert!(matches!(error, Some(Error::Unavailable)), "{error:?}");
}

#[tokio::test]
async fn a_stream_hands_chunks_over_as_they_arrive_and_an_oversize_plain_answer_is_refused() {
    // 청크 전송 응답: 조각을 순서대로 내주고 끝나면 None
    let chunked = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n6\r\nevent1\r\n6\r\nevent2\r\n0\r\n\r\n".to_vec();
    let server = serve(move |_| chunked.clone()).await;
    let mut stream = upstream()
        .stream(request(&server, &[], "{}"))
        .await
        .unwrap();
    assert_eq!(stream.status, 200);
    let mut all = Vec::new();
    while let Some(chunk) = stream.body.next().await.unwrap() {
        all.extend(chunk);
    }
    assert_eq!(String::from_utf8(all).unwrap(), "event1event2");
    // 한도(시험에서는 1024바이트)를 넘는 일반 응답은 받지 않는다
    let big = "x".repeat(2000);
    let server = serve(move |_| http("200 OK", "", &big)).await;
    let error = upstream().send(request(&server, &[], "{}")).await.err();
    assert!(matches!(error, Some(Error::Unavailable)), "{error:?}");
}

#[tokio::test]
async fn bad_headers_never_reach_the_network_and_ipv6_hosts_are_bracketed() {
    let server = serve(|_| http("200 OK", "", "{}")).await;
    for (name, value) in [("bad name", "v"), ("x-ok", "line\nbreak"), ("", "v")] {
        let error = upstream()
            .send(request(&server, &[(name, value)], "{}"))
            .await
            .err();
        assert!(
            matches!(error, Some(Error::Invalid)),
            "{name:?} {value:?} → {error:?}"
        );
    }
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        0,
        "연결하지 않았어요"
    );
    let https = HttpsUpstream::new().unwrap();
    let mut call = request(&server, &[], "{}");
    call.endpoint = HostPort {
        host: "api.example".into(),
        port: 443,
    };
    assert_eq!(https.url(&call), "https://api.example:443/v1/messages");
    call.endpoint = HostPort {
        host: "fd00::1".into(),
        port: 8443,
    };
    assert_eq!(https.url(&call), "https://[fd00::1]:8443/v1/messages");
}
