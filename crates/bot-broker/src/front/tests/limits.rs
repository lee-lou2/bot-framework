//! Request limits, routing, chunked decoding, revocation, and ledger outage tests.

use super::*;

#[tokio::test]
async fn unknown_routes_and_methods_get_fixed_errors() {
    let r = plain().await;
    let get = |path: &str| format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").into_bytes();
    assert_eq!(raw(r.front.addr(), &get("/mcp")).await.status, 405);
    for path in [
        "/",
        "/v1/models",
        "/v1/messages/count_tokens",
        "/admin",
        "/mcp/../v1/messages",
    ] {
        assert_eq!(
            raw(
                r.front.addr(),
                &post(path, Some(&r.token), "x-api-key", "{}")
            )
            .await
            .status,
            404,
            "{path}"
        );
    }
    assert_eq!(raw(r.front.addr(), &get("/v1/messages")).await.status, 404);
    assert!(r.upstream.requests().is_empty());
}

#[tokio::test]
async fn request_limits_apply_before_anything_is_run() {
    let r = rig(
        |c| {
            c.max_body = 64;
            c.head_timeout = Duration::from_millis(150);
            c.body_timeout = Duration::from_millis(150);
        },
        |_| {},
    )
    .await;
    let big = post(
        "/v1/messages",
        Some(&r.token),
        "x-api-key",
        &format!("{{\"stream\":true,\"x\":\"{}\"}}", "a".repeat(100)),
    );
    assert_eq!(raw(r.front.addr(), &big).await.status, 413);
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: x\r\nx-pad: {}\r\n\r\n",
        "a".repeat(20_000)
    );
    assert_eq!(raw(r.front.addr(), head.as_bytes()).await.status, 431);
    for bad in [
        "POST /mcp HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello",
        "POST /mcp HTTP/1.1\r\nContent-Length: -1\r\n\r\n",
        "POST /mcp HTTP/1.1\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        "POST /mcp HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n",
        "GARBAGE\r\n\r\n",
        "POST mcp HTTP/1.1\r\n\r\n",
        "POST /mcp HTTP/1.1\r\nbad header\r\n\r\n",
        "POST /mcp HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcdef",
    ] {
        assert_eq!(
            raw(r.front.addr(), bad.as_bytes()).await.status,
            400,
            "{bad:?}"
        );
    }
    assert_eq!(
        raw(r.front.addr(), b"POST /mcp HTTP/1.1\r\nHost: x")
            .await
            .status,
        408
    );
    assert_eq!(
        raw(
            r.front.addr(),
            b"POST /mcp HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc"
        )
        .await
        .status,
        408
    );
    assert!(r.upstream.requests().is_empty() && r.runner.jobs().is_empty());
}

#[tokio::test]
async fn chunked_request_bodies_are_decoded_and_bad_chunks_are_refused() {
    let r = rig(|c| c.body_timeout = Duration::from_millis(300), |_| {}).await;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let (a, b) = body.split_at(10);
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nTransfer-Encoding: chunked\r\n\r\n",
        r.token
    );
    let good = format!(
        "{head}{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
        a.len(),
        b.len()
    );
    let reply = raw(r.front.addr(), good.as_bytes()).await;
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("echo_say"));
    for bad in [
        "zz\r\nabc\r\n0\r\n\r\n",
        "3\r\nabcde\r\n0\r\n\r\n",
        "3\r\nabc\r\n",
        "ffffffff\r\n",
    ] {
        let reply = raw(r.front.addr(), format!("{head}{bad}").as_bytes()).await;
        assert!(
            matches!(reply.status, 400 | 408 | 413),
            "{bad:?}: {}",
            reply.status
        );
    }
}

#[tokio::test]
async fn a_client_that_leaves_mid_stream_gives_back_its_slot_and_closes_the_upstream() {
    let r = rig(
        |_| {},
        |c| {
            c.rate = RateConfig {
                per_minute: 6000,
                burst: 100,
                concurrent: 1,
            }
        },
    )
    .await;
    r.upstream
        .respond_chunks(200, &[b"data: {}\n\n"], End::Hang);
    let mut stream = TcpStream::connect(r.front.addr()).await.unwrap();
    stream
        .write_all(&messages(&r, r#"{"stream":true}"#))
        .await
        .unwrap();
    let mut seen = [0u8; 512];
    timeout(Duration::from_secs(5), stream.read(&mut seen))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        raw(r.front.addr(), &messages(&r, r#"{"stream":true}"#))
            .await
            .status,
        503
    );
    drop(stream);
    let mut freed = false;
    for _ in 0..100 {
        r.upstream.respond_chunks(200, &[b"data: {}\n\n"], End::Eof);
        if raw(r.front.addr(), &messages(&r, r#"{"stream":true}"#))
            .await
            .status
            == 200
        {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        freed,
        "concurrency slot must be released after client disconnect"
    );
}

#[tokio::test]
async fn stopping_the_front_closes_the_port() {
    let r = plain().await;
    let addr = r.front.addr();
    r.front.stop().await;
    assert!(TcpStream::connect(addr).await.is_err());
}

async fn read_all(stream: &mut TcpStream) -> String {
    let mut all = Vec::new();
    let _ = timeout(Duration::from_secs(5), stream.read_to_end(&mut all)).await;
    String::from_utf8_lossy(&all).into_owned()
}

async fn open_stream(r: &Rig) -> TcpStream {
    r.upstream
        .respond_chunks(200, &[b"data: {}\n\n"], End::Hang);
    let mut stream = TcpStream::connect(r.front.addr()).await.unwrap();
    stream
        .write_all(&messages(r, r#"{"stream":true}"#))
        .await
        .unwrap();
    let mut seen = [0u8; 512];
    timeout(Duration::from_secs(5), stream.read(&mut seen))
        .await
        .unwrap()
        .unwrap();
    stream
}

fn one_slot(c: &mut ModelLimits) {
    c.rate = RateConfig {
        per_minute: 6000,
        burst: 100,
        concurrent: 1,
    };
}

async fn slot_is_free(r: &Rig) -> bool {
    slot_is_free_for(r, r.attempt.clone()).await
}

async fn slot_is_free_for(r: &Rig, attempt: Attempt) -> bool {
    let token = r
        .sessions
        .issue(Session {
            attempt,
            agent: AgentId("main".into()),
            alias: "assistant".into(),
        })
        .unwrap();
    let request = post(
        "/v1/messages",
        Some(&token),
        "x-api-key",
        r#"{"stream":true}"#,
    );
    for _ in 0..100 {
        r.upstream.respond_chunks(200, &[b"data: {}\n\n"], End::Eof);
        if raw(r.front.addr(), &request).await.status == 200 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn revoking_the_token_ends_an_open_stream_with_one_fixed_error_and_frees_the_slot() {
    let r = rig(|_| {}, one_slot).await;
    let mut stream = open_stream(&r).await;
    r.sessions.revoke(&r.token);
    let rest = read_all(&mut stream).await;
    assert!(
        rest.contains(r#""message":"stale""#) && rest.contains("permission_error"),
        "{rest}"
    );
    assert!(
        rest.ends_with("0\r\n\r\n"),
        "stream must end with a terminating chunk: {rest:?}"
    );
    assert!(
        slot_is_free(&r).await,
        "upstream must close and free the slot"
    );
}

#[tokio::test]
async fn revoking_the_attempt_ends_its_stream_but_not_another_claims_session() {
    let r = rig(|_| {}, one_slot).await;
    let mut stream = open_stream(&r).await;
    let mut newer = r.attempt.clone();
    newer.claim = Some(ClaimEpoch(2));
    r.sessions.revoke_attempt(&newer);
    assert!(
        timeout(Duration::from_millis(300), read_all(&mut stream))
            .await
            .is_err(),
        "revoking another claim must not end this stream"
    );
    r.sessions.revoke_attempt(&r.attempt);
    assert!(read_all(&mut stream).await.contains(r#""message":"stale""#));
}

#[tokio::test]
async fn a_run_that_changed_hands_ends_its_open_stream_at_the_next_check() {
    let r = rig(|f| f.revoke_check = Duration::from_millis(50), one_slot).await;
    let mut stream = open_stream(&r).await;
    r.ledger.bump_claim("r1");
    let rest = read_all(&mut stream).await;
    assert!(rest.contains(r#""message":"stale""#), "{rest}");
    assert!(rest.ends_with("0\r\n\r\n"), "{rest:?}");
    let newer = Attempt {
        claim: Some(ClaimEpoch(r.ledger.claim_epoch("r1").0)),
        ..r.attempt.clone()
    };
    assert!(slot_is_free_for(&r, newer).await);
}

#[tokio::test]
async fn a_plain_request_waiting_for_the_upstream_is_dropped_when_the_session_is_revoked() {
    let r = rig(|_| {}, one_slot).await;
    r.upstream.hang();
    let mut waiting = TcpStream::connect(r.front.addr()).await.unwrap();
    waiting
        .write_all(&messages(&r, r#"{"messages":[]}"#))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    r.sessions.revoke(&r.token);
    let reply = read_all(&mut waiting).await;
    assert!(
        reply.starts_with("HTTP/1.1 403") && reply.contains(r#""message":"stale""#),
        "{reply}"
    );
    assert!(slot_is_free(&r).await, "waiting request must free its slot");
}

async fn drain(stream: &mut TcpStream, got: &mut String, ms: u64) {
    let mut buf = [0u8; 4096];
    let until = tokio::time::Instant::now() + Duration::from_millis(ms);
    while let Ok(Ok(n)) = tokio::time::timeout_at(until, stream.read(&mut buf)).await {
        if n == 0 {
            break;
        }
        got.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
}

fn numbered(n: usize) -> Vec<Vec<u8>> {
    [b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_n\"}}\n\n".to_vec()]
        .into_iter()
        .chain((0..n).map(|i| format!("data: {{\"n\":{i}}}\n\n").into_bytes()))
        .chain([b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec()])
        .collect()
}

fn seen(got: &str) -> usize {
    got.matches("\"n\":").count()
}

#[tokio::test]
async fn an_outage_pauses_the_output_and_a_recovery_resumes_it_without_loss() {
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(30);
            f.verify_grace = Duration::from_secs(5);
        },
        one_slot,
    )
    .await;
    let events = numbered(8);
    let chunks: Vec<&[u8]> = events.iter().map(Vec::as_slice).collect();
    r.upstream
        .respond_slowly(200, &chunks, Duration::from_millis(100), End::Eof);
    let mut stream = TcpStream::connect(r.front.addr()).await.unwrap();
    stream
        .write_all(&messages(&r, r#"{"stream":true}"#))
        .await
        .unwrap();
    let mut got = String::new();
    for _ in 0..60 {
        if seen(&got) >= 1 {
            break;
        }
        drain(&mut stream, &mut got, 50).await;
    }
    assert!(seen(&got) >= 1, "{got}");
    r.ledger.set_context_down(true);
    drain(&mut stream, &mut got, 250).await;
    let paused_at = seen(&got);
    drain(&mut stream, &mut got, 400).await;
    assert_eq!(seen(&got), paused_at, "output must stay paused: {got}");
    assert!(paused_at < 8, "{got}");
    r.ledger.set_context_down(false);
    drain(&mut stream, &mut got, 1500).await;
    assert_eq!(seen(&got), 8, "{got}");
    assert!(!got.contains("\"error\""), "{got}");
    assert!(got.ends_with("0\r\n\r\n"), "{got:?}");
}

#[tokio::test]
async fn an_outage_past_the_grace_closes_the_stream_with_one_fixed_error() {
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(30);
            f.verify_grace = Duration::from_millis(250);
        },
        one_slot,
    )
    .await;
    let mut stream = open_stream(&r).await;
    r.ledger.set_context_down(true);
    let rest = read_all(&mut stream).await;
    assert!(
        rest.contains(r#""message":"unavailable""#) && rest.ends_with("0\r\n\r\n"),
        "{rest:?}"
    );
    r.ledger.set_context_down(false);
    assert!(slot_is_free(&r).await, "slot must be released after close");
}

#[tokio::test]
async fn a_rejection_after_an_outage_still_ends_the_stream() {
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(30);
            f.verify_grace = Duration::from_secs(5);
        },
        one_slot,
    )
    .await;
    let mut stream = open_stream(&r).await;
    r.ledger.set_context_down(true);
    tokio::time::sleep(Duration::from_millis(150)).await;
    r.ledger.bump_claim("r1");
    r.ledger.set_context_down(false);
    assert!(read_all(&mut stream).await.contains(r#""message":"stale""#));
}

#[tokio::test]
async fn a_plain_response_is_held_back_while_ownership_cannot_be_confirmed() {
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(30);
            f.verify_grace = Duration::from_millis(400);
        },
        one_slot,
    )
    .await;
    r.upstream.set_delay(Duration::from_millis(250));
    let ask = |r: &Rig| {
        let (addr, request) = (r.front.addr(), messages(r, r#"{"messages":[]}"#));
        tokio::spawn(async move { raw(addr, &request).await })
    };
    let waiting = ask(&r);
    tokio::time::sleep(Duration::from_millis(80)).await;
    r.ledger.set_context_down(true);
    let reply = waiting.await.unwrap();
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert!(
        reply.body.contains("unavailable") && !reply.body.contains("\"ok\":true"),
        "{}",
        reply.body
    );
    r.ledger.set_context_down(false);
    let waiting = ask(&r);
    tokio::time::sleep(Duration::from_millis(80)).await;
    r.ledger.set_context_down(true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    r.ledger.set_context_down(false);
    let reply = waiting.await.unwrap();
    assert_eq!((reply.status, reply.body.as_str()), (200, "{\"ok\":true}"));
}

#[tokio::test]
async fn a_slow_ledger_and_a_fast_stream_still_notice_a_lost_claim() {
    // Events arrive every 10ms while ledger checks take 100ms; verifies checks are not cancelled per event.
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(20);
            f.verify_grace = Duration::from_secs(5);
        },
        one_slot,
    )
    .await;
    let events = numbered(300);
    let chunks: Vec<&[u8]> = events.iter().map(Vec::as_slice).collect();
    r.upstream
        .respond_slowly(200, &chunks, Duration::from_millis(10), End::Eof);
    r.ledger.set_context_delay(Duration::from_millis(100));
    let mut stream = TcpStream::connect(r.front.addr()).await.unwrap();
    stream
        .write_all(&messages(&r, r#"{"stream":true}"#))
        .await
        .unwrap();
    let mut got = String::new();
    drain(&mut stream, &mut got, 600).await;
    assert!(seen(&got) >= 5, "stream should be flowing: {}", seen(&got));
    r.ledger.bump_claim("r1");
    drain(&mut stream, &mut got, 2_000).await;
    assert!(got.contains(r#""message":"stale""#), "{got}");
    assert!(seen(&got) < 250, "stream ran to completion: {}", seen(&got));
}

#[tokio::test]
async fn an_outage_seen_while_waiting_for_the_upstream_holds_back_the_head_and_the_body() {
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(20);
            f.verify_grace = Duration::from_millis(500);
        },
        one_slot,
    )
    .await;
    let events = numbered(3);
    let chunks: Vec<&[u8]> = events.iter().map(Vec::as_slice).collect();
    r.upstream.set_delay(Duration::from_millis(300));
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut stream = TcpStream::connect(r.front.addr()).await.unwrap();
    stream
        .write_all(&messages(&r, r#"{"stream":true}"#))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    r.ledger.set_context_down(true);
    let mut got = String::new();
    drain(&mut stream, &mut got, 420).await;
    assert!(
        got.is_empty(),
        "neither headers nor body may flush while unverified: {got}"
    );
    r.ledger.set_context_down(false);
    drain(&mut stream, &mut got, 800).await;
    assert!(got.starts_with("HTTP/1.1 200") && seen(&got) == 3, "{got}");
}

#[tokio::test]
async fn an_outage_that_outlasts_the_grace_while_waiting_closes_with_one_fixed_error() {
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(20);
            f.verify_grace = Duration::from_millis(250);
        },
        one_slot,
    )
    .await;
    r.upstream.set_delay(Duration::from_millis(300));
    r.upstream
        .respond_chunks(200, &[b"data: {\"n\":0}\n\n"], End::Eof);
    let mut stream = TcpStream::connect(r.front.addr()).await.unwrap();
    stream
        .write_all(&messages(&r, r#"{"stream":true}"#))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    r.ledger.set_context_down(true);
    let reply = read_all(&mut stream).await;
    assert!(
        reply.contains("503") && reply.contains("unavailable") && !reply.contains("\"n\":0"),
        "{reply}"
    );
}

#[tokio::test]
async fn a_routine_check_that_runs_past_its_own_cap_is_a_timeout_even_if_it_succeeds() {
    // Routine ownership checks cap at max(revoke_check, 1s); a stalled poll exceeding that cap expires grace.
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(20);
            f.verify_grace = Duration::from_millis(500);
        },
        one_slot,
    )
    .await;
    let events = numbered(400);
    let chunks: Vec<&[u8]> = events.iter().map(Vec::as_slice).collect();
    r.upstream
        .respond_slowly(200, &chunks, Duration::from_millis(10), End::Eof);
    let mut stream = TcpStream::connect(r.front.addr()).await.unwrap();
    stream
        .write_all(&messages(&r, r#"{"stream":true}"#))
        .await
        .unwrap();
    let mut got = String::new();
    for _ in 0..60 {
        if seen(&got) >= 1 {
            break;
        }
        drain(&mut stream, &mut got, 50).await;
    }
    assert!(seen(&got) >= 1, "{got}");
    r.ledger.set_context_block(Duration::from_millis(1_150));
    drain(&mut stream, &mut got, 4_000).await;
    r.ledger.set_context_block(Duration::ZERO);
    assert!(
        got.contains(r#""message":"unavailable""#) && seen(&got) < 390,
        "{} events: {}",
        seen(&got),
        &got[got.len().saturating_sub(300)..]
    );
}
