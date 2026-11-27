//! Ownership verification and timeout behavior during and after tool-turn publication.

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
    let gate = r.turns.hold_next();
    let events = tools_response("msg_1");
    let chunks: Vec<&[u8]> = events.iter().map(Vec::as_slice).collect();
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut client = open(&r).await;
    r.turns.entered().await;
    r.ledger.bump_claim("r1");
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
            // Slow periodic check ensures the pre-release confirmation itself blocks emission.
            f.revoke_check = Duration::from_millis(500);
            f.verify_grace = Duration::from_millis(400);
        },
        |_| {},
    )
    .await;
    let events = tools_response("msg_1");
    let chunks: Vec<&[u8]> = events.iter().map(Vec::as_slice).collect();
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
    // Recovering within the grace window releases `message_stop`.
    r.ledger.set_context_down(false);
    let gate = r.turns.hold_next();
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut client = open(&r).await;
    r.turns.entered().await;
    r.ledger.set_context_down(true);
    gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
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
    assert_eq!(reply.status, 503, "{}", reply.body);
    r.ledger.set_context_down(false);
    r.upstream.set_delay(Duration::from_millis(120));
    r.upstream.respond_chunks(429, &[b"{}"], End::Eof);
    let mut client = open(&r).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    r.ledger.set_context_down(true);
    let reply = all(&mut client).await;
    assert!(reply.contains("503") && !reply.contains("429"), "{reply}");
}

/// Slows the ledger after `prepare` so delay falls on the post-upstream ownership confirmation.
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
    r.ledger.set_context_delay(Duration::from_millis(ledger_ms));
    waiting.await.unwrap()
}

#[tokio::test]
async fn a_ledger_that_answers_after_the_grace_does_not_release_the_upstream_status() {
    let reply = status_with_slow_ledger(80, 350).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert!(!reply.body.contains("429"), "{}", reply.body);
    let reply = status_with_slow_ledger(900, 350).await;
    assert_eq!(reply.status, 429, "{}", reply.body);
}

#[tokio::test]
async fn the_pause_between_retries_never_runs_past_the_deadline() {
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
    // Recover only after the 100ms grace deadline has elapsed.
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
    tokio::time::sleep(Duration::from_millis(250)).await;
    r.ledger.set_context_down(false);
    let reply = waiting.await.unwrap();
    assert_eq!(reply.status, 429, "{}", reply.body);
}

#[tokio::test]
async fn a_confirmation_that_lands_after_the_deadline_is_not_accepted_even_if_it_succeeds() {
    // Block the thread past the 80ms grace deadline so both lookup and deadline become ready together.
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
    r.ledger.set_context_block(Duration::from_millis(150));
    let reply = waiting.await.unwrap();
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert!(!reply.body.contains("429"), "{}", reply.body);
}
