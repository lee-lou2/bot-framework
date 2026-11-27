//! No hidden retries: a request closed after write without a response settles as `Raw::Unknown` and is never resent.
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

/// TCP server that reads a full JSON request and closes without sending a response.
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
                    // Headers and JSON body (`}`) received: count one request and close.
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
    let body = json!({"channel": "C1", "text": "hello"});
    let outcome = api
        .call("chat.postMessage", "test-token", Body::Json(&body))
        .await;
    assert_eq!(outcome, Raw::Unknown);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "must not resend the same request"
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
