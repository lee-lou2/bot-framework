//! 숨은 재전송 금지: 요청을 다 보낸 뒤 응답 없이 연결이 끊겨도 같은 요청을 다시 보내지 않는다(결과는 불명으로 정산).
use bot_gateway::slack::api::{Body, Raw, SlackApi};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// 요청 본문까지 읽은 뒤 응답 없이 닫는 서버. 받은 요청 수를 센다.
async fn silent_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/", listener.local_addr().unwrap());
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                let mut total = Vec::new();
                loop {
                    let n = stream.read(&mut buffer).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    total.extend_from_slice(&buffer[..n]);
                    // 헤더와 본문(JSON 끝 `}`)이 모두 왔으면 요청 하나를 받은 것이다
                    if total.windows(4).any(|w| w == b"\r\n\r\n") && total.ends_with(b"}") {
                        counter.fetch_add(1, Ordering::SeqCst);
                        let _ = stream.shutdown().await;
                        return;
                    }
                }
            });
        }
    });
    (base, seen)
}

#[tokio::test]
async fn a_request_whose_answer_was_lost_is_never_sent_twice() {
    let (base, seen) = silent_server().await;
    let api = SlackApi::new(Some(&base));
    let body = json!({"channel": "C1", "text": "안녕"});
    let outcome = api
        .call("chat.postMessage", "test-token", Body::Json(&body))
        .await;
    // 요청은 나갔고 응답을 모르니 불명이다
    assert_eq!(outcome, Raw::Unknown);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "같은 요청을 다시 보내면 안 돼요"
    );
}

#[tokio::test]
async fn a_refused_connection_is_reported_as_before_write_without_retries() {
    let api = SlackApi::new(Some("http://127.0.0.1:1/api/"));
    let body = json!({"channel": "C1", "text": "x"});
    assert_eq!(
        api.call("chat.postMessage", "test-token", Body::Json(&body))
            .await,
        Raw::BeforeWrite
    );
}
