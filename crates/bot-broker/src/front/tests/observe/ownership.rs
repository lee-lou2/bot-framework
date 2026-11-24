//! 도구 목록을 저장하는 동안·저장한 뒤의 소유권 확인과 시간 한도.

use super::*;

fn tools_response(id: &str) -> Vec<Vec<u8>> {
    let mut events = vec![start(id)];
    events.extend(tool_block(0, "toolu_a", "echo_say", &["{}"]));
    events.extend(tail());
    events
}

async fn open(r: &Rig) -> TcpStream {
    let mut client = TcpStream::connect(r.front.addr()).await.unwrap();
    client
        .write_all(&messages(r, r#"{"stream":true,"messages":[]}"#))
        .await
        .unwrap();
    client
}

async fn all(client: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.read_to_end(&mut bytes),
    )
    .await;
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn losing_the_claim_while_the_tools_are_being_stored_never_releases_the_stop_event() {
    let r = rig(|f| f.revoke_check = Duration::from_millis(20), |_| {}).await;
    let gate = r.turns.hold_next(); // 저장이 끝나지 않는 저장소
    let events = tools_response("msg_1");
    let chunks: Vec<&[u8]> = events.iter().map(Vec::as_slice).collect();
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut client = open(&r).await;
    r.turns.entered().await; // 저장에 들어선 순간
    r.ledger.bump_claim("r1"); // 저장하는 동안 claim이 넘어갔다
    let reply = all(&mut client).await;
    assert!(
        !reply.contains("message_stop") && reply.contains("\"message\":\"stale\""),
        "{reply}"
    );
    drop(gate);
}

#[tokio::test]
async fn storing_the_tools_has_a_time_limit() {
    let r = rig(|f| f.publish_timeout = Duration::from_millis(150), |_| {}).await;
    r.turns.set_delay(Duration::from_secs(5));
    let events = tools_response("msg_1");
    let chunks: Vec<&[u8]> = events.iter().map(Vec::as_slice).collect();
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut client = open(&r).await;
    let reply = all(&mut client).await;
    assert!(
        !reply.contains("message_stop") && reply.contains("\"message\":\"unavailable\""),
        "{reply}"
    );
    // 일반 응답도 같다
    r.upstream.respond(
        200,
        br#"{"id":"msg_9","content":[{"type":"tool_use","id":"toolu_n","name":"echo_say","input":{}}]}"#,
    );
    let reply = raw(r.front.addr(), &messages(&r, r#"{"messages":[]}"#)).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
}

#[tokio::test]
async fn the_stop_event_waits_for_a_fresh_confirmation_after_the_tools_are_stored() {
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(500); // 주기 확인은 느리다: 내보내기 직전 확인이 막아야 한다
            f.verify_grace = Duration::from_millis(400);
        },
        |_| {},
    )
    .await;
    let events = tools_response("msg_1");
    let chunks: Vec<&[u8]> = events.iter().map(Vec::as_slice).collect();
    // 저장에 들어선 순간 원장을 멈추고, 그 상태에서 저장이 끝나게 한다
    let gate = r.turns.hold_next();
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut client = open(&r).await;
    r.turns.entered().await;
    r.ledger.set_context_down(true);
    gate.add_permits(1);
    let reply = all(&mut client).await;
    assert!(
        !reply.contains("message_stop") && reply.contains("\"message\":\"unavailable\""),
        "{reply}"
    );
    // 확인이 돌아오면(마감 안에) 그대로 내보낸다
    r.ledger.set_context_down(false);
    let gate = r.turns.hold_next();
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut client = open(&r).await;
    r.turns.entered().await;
    r.ledger.set_context_down(true);
    gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await; // 마감(400ms)보다 훨씬 짧게만 멈춘다
    r.ledger.set_context_down(false);
    let reply = all(&mut client).await;
    assert!(
        reply.contains("message_stop") && !reply.contains("\"error\""),
        "{reply}"
    );
}

#[tokio::test]
async fn an_upstream_error_status_is_released_only_after_a_fresh_confirmation() {
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(500);
            f.verify_grace = Duration::from_millis(300);
        },
        |_| {},
    )
    .await;
    r.upstream.set_delay(Duration::from_millis(100));
    r.upstream.respond(429, b"{}");
    let waiting = {
        let (addr, request) = (r.front.addr(), messages(&r, r#"{"messages":[]}"#));
        tokio::spawn(async move { raw(addr, &request).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    r.ledger.set_context_down(true);
    let reply = waiting.await.unwrap();
    assert_eq!(reply.status, 503, "{}", reply.body); // 상류의 429가 아니라, 확인할 수 없어 닫은 고정 오류다
    // 스트림 요청의 상류 오류 상태도 같다
    r.ledger.set_context_down(false);
    r.upstream.set_delay(Duration::from_millis(120)); // 스트림 응답 머리가 늦게 온다
    r.upstream.respond_chunks(429, &[b"{}"], End::Eof);
    let mut client = open(&r).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    r.ledger.set_context_down(true);
    let reply = all(&mut client).await;
    assert!(reply.contains("503") && !reply.contains("429"), "{reply}");
}

/// 상류 오류 상태 응답을 받은 직후의 소유권 확인에 걸리는 시간을 만든다: 요청이 상류로 간 뒤(`prepare`를 지난 뒤)에 원장을 느리게 한다.
async fn status_with_slow_ledger(grace_ms: u64, ledger_ms: u64) -> Reply {
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(500);
            f.verify_grace = Duration::from_millis(grace_ms);
        },
        |_| {},
    )
    .await;
    r.upstream.set_delay(Duration::from_millis(150));
    r.upstream.respond(429, b"{}");
    let waiting = {
        let (addr, request) = (r.front.addr(), messages(&r, r#"{"messages":[]}"#));
        tokio::spawn(async move { raw(addr, &request).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    r.ledger.set_context_delay(Duration::from_millis(ledger_ms)); // 응답이 오기 전에 원장이 느려진다
    waiting.await.unwrap()
}

#[tokio::test]
async fn a_ledger_that_answers_after_the_grace_does_not_release_the_upstream_status() {
    // 유예 80ms, 원장 350ms: 마감(80ms)을 넘겨 답한 확인은 받아들이지 않는다 → 고정 503
    let reply = status_with_slow_ledger(80, 350).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert!(!reply.body.contains("429"), "{}", reply.body);
    // 유예가 충분하면(원장 350ms < 유예 900ms) 확인된 뒤에 상류 상태를 그대로 내보낸다
    let reply = status_with_slow_ledger(900, 350).await;
    assert_eq!(reply.status, 429, "{}", reply.body);
}

#[tokio::test]
async fn the_pause_between_retries_never_runs_past_the_deadline() {
    // 주기 확인(revoke_check)이 길어도(500ms) 재시도 사이의 쉼이 마감(유예 100ms)을 넘기지 않는다: 늦게 성공해도 503
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(500);
            f.verify_grace = Duration::from_millis(100);
        },
        |_| {},
    )
    .await;
    r.upstream.set_delay(Duration::from_millis(100));
    r.upstream.respond(429, b"{}");
    let waiting = {
        let (addr, request) = (r.front.addr(), messages(&r, r#"{"messages":[]}"#));
        tokio::spawn(async move { raw(addr, &request).await })
    };
    tokio::time::sleep(Duration::from_millis(40)).await;
    r.ledger.set_context_down(true);
    // 마감(응답이 온 ~100ms부터 100ms) 뒤에야 원장이 돌아온다
    tokio::time::sleep(Duration::from_millis(250)).await;
    r.ledger.set_context_down(false);
    let reply = waiting.await.unwrap();
    assert_eq!(reply.status, 503, "{}", reply.body);
}

#[tokio::test]
async fn a_recovery_inside_the_grace_is_seen_by_a_retry_before_the_deadline() {
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(500);
            f.verify_grace = Duration::from_millis(600);
        },
        |_| {},
    )
    .await;
    r.upstream.set_delay(Duration::from_millis(100));
    r.upstream.respond(429, b"{}");
    let waiting = {
        let (addr, request) = (r.front.addr(), messages(&r, r#"{"messages":[]}"#));
        tokio::spawn(async move { raw(addr, &request).await })
    };
    tokio::time::sleep(Duration::from_millis(40)).await;
    r.ledger.set_context_down(true);
    tokio::time::sleep(Duration::from_millis(250)).await; // 유예(600ms) 안에서 돌아온다
    r.ledger.set_context_down(false);
    let reply = waiting.await.unwrap();
    assert_eq!(reply.status, 429, "{}", reply.body);
}

#[tokio::test]
async fn a_confirmation_that_lands_after_the_deadline_is_not_accepted_even_if_it_succeeds() {
    // 실행이 밀려 마감과 조회가 함께 준비되면 조회 성공이 먼저 나온다: 조회가 마감을 넘겨 끝났으면 받아들이지 않는다
    let r = rig(
        |f| {
            f.revoke_check = Duration::from_millis(500);
            f.verify_grace = Duration::from_millis(80);
        },
        |_| {},
    )
    .await;
    r.upstream.set_delay(Duration::from_millis(100));
    r.upstream.respond(429, b"{}");
    let waiting = {
        let (addr, request) = (r.front.addr(), messages(&r, r#"{"messages":[]}"#));
        tokio::spawn(async move { raw(addr, &request).await })
    };
    tokio::time::sleep(Duration::from_millis(40)).await;
    r.ledger.set_context_block(Duration::from_millis(150)); // 조회가 스레드를 붙잡고 마감(80ms)을 넘겨 성공한다
    let reply = waiting.await.unwrap();
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert!(!reply.body.contains("429"), "{}", reply.body);
}
