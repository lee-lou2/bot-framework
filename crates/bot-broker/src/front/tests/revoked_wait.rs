//! Session revocation aborts in-flight waits immediately and prevents any tool execution or delegation.

use super::{
    cli::{OWNER_RUN, call, planned, record},
    delegate::{delegate, family, plan},
    *,
};
use crate::front::mcp::unless_revoked;
use std::time::Instant;
use tokio::{sync::watch, task::JoinHandle};

fn send_later(r: &Rig, body: &str) -> JoinHandle<(Reply, Duration)> {
    let token = format!("Bearer {}", r.token);
    let (addr, request) = (
        r.front.addr(),
        post("/mcp", Some(&token), "authorization", body),
    );
    tokio::spawn(async move {
        let began = Instant::now();
        (raw(addr, &request).await, began.elapsed())
    })
}

fn nothing_started(r: &Rig, call_id: &str) {
    assert!(r.runner.jobs().is_empty(), "tool must not execute");
    assert!(
        !r.ledger.has_call(OWNER_RUN, call_id),
        "call record must not be created"
    );
}

// ---- unless_revoked ----

#[tokio::test]
async fn a_wait_that_ends_normally_gives_its_output() {
    let (_tx, gone) = watch::channel(false);
    assert_eq!(unless_revoked(&gone, async { 7 }).await, Some(7));
}

#[tokio::test]
async fn a_revocation_during_the_wait_drops_it_at_once() {
    let (tx, gone) = watch::channel(false);
    let waiting =
        tokio::spawn(async move { unless_revoked(&gone, std::future::pending::<()>()).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    tx.send(true).unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("wait should end immediately upon revocation")
        .unwrap();
    assert_eq!(ended, None);
}

#[tokio::test]
async fn a_revocation_that_lands_as_the_wait_ends_still_wins() {
    // Revoke inside the poll that completes the future so only the post-poll check catches it.
    let (tx, gone) = watch::channel(false);
    let output = unless_revoked(&gone, async move {
        tx.send(true).unwrap();
        7
    })
    .await;
    assert_eq!(output, None);
}

#[tokio::test]
async fn a_session_that_the_table_dropped_counts_as_revoked() {
    let (tx, gone) = watch::channel(false);
    drop(tx);
    assert_eq!(unless_revoked(&gone, async { 7 }).await, None);
    // Dropping the sender also cancels a pending wait immediately.
    let (tx, gone) = watch::channel(false);
    let waiting =
        tokio::spawn(async move { unless_revoked(&gone, std::future::pending::<()>()).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(tx);
    let ended = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("wait should end immediately when session is dropped")
        .unwrap();
    assert_eq!(ended, None);
}

// ---- call path ----

#[tokio::test]
async fn revoking_while_the_refusal_marker_is_read_answers_at_once_and_runs_nothing() {
    let r = rig_with(|_| {}, |_| {}, planned(5_000)).await;
    record(&r, 4, &[("toolu_a", "echo_say", "{}")]);
    // Hold the refusal-marker read after the plan is loaded.
    let gate = r.ledger.hold_next_step("cli-refusal-");
    let waiting = send_later(&r, &call("toolu_a", "echo_say", "{}"));
    r.ledger.step_held().await;
    r.sessions.revoke(&r.token);
    let (reply, took) = waiting.await.unwrap();
    assert!(reply.body.contains("stale"), "{}", reply.body);
    assert!(
        took < Duration::from_secs(2),
        "should return without waiting for the read to finish: {took:?}"
    );
    // Releasing the read afterward must not start execution on the revoked session.
    gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    nothing_started(&r, "cli-4-0");
}

#[tokio::test]
async fn a_revocation_that_lands_as_the_marker_read_returns_still_runs_nothing() {
    let r = rig_with(|_| {}, |_| {}, planned(5_000)).await;
    record(&r, 4, &[("toolu_a", "echo_say", "{}")]);
    // Revoke at the exact moment the marker read returns None.
    let (sessions, token) = (r.sessions.clone(), r.token.clone());
    r.ledger
        .after_next_step("cli-refusal-", move || sessions.revoke(&token));
    let reply = rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    assert!(reply.body.contains("stale"), "{}", reply.body);
    nothing_started(&r, "cli-4-0");
}

#[tokio::test]
async fn revoking_while_the_marker_is_read_creates_no_delegation() {
    let r = rig_with(|_| {}, |_| {}, planned(5_000)).await;
    r.catalog.publish(family("r1").build());
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    plan(&r, 4, "toolu_d", input);
    let gate = r.ledger.hold_next_step("cli-refusal-");
    let waiting = send_later(&r, &delegate("toolu_d", input));
    r.ledger.step_held().await;
    r.sessions.revoke(&r.token);
    let (reply, took) = waiting.await.unwrap();
    assert!(reply.body.contains("stale"), "{}", reply.body);
    assert!(took < Duration::from_secs(2), "{took:?}");
    gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        r.delegation.commands().is_empty(),
        "child delegation must not be created"
    );
    nothing_started(&r, "cli-4-0");
}

#[tokio::test]
async fn a_revocation_that_lands_as_the_marker_read_returns_creates_no_delegation() {
    let r = rig_with(|_| {}, |_| {}, planned(5_000)).await;
    r.catalog.publish(family("r1").build());
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    plan(&r, 4, "toolu_d", input);
    let (sessions, token) = (r.sessions.clone(), r.token.clone());
    r.ledger
        .after_next_step("cli-refusal-", move || sessions.revoke(&token));
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(reply.body.contains("stale"), "{}", reply.body);
    assert!(
        r.delegation.commands().is_empty(),
        "child delegation must not be created"
    );
    nothing_started(&r, "cli-4-0");
}

fn position(call: &str) -> crate::front::slots::Key {
    (
        ProfileId("personal".into()),
        RunId(OWNER_RUN.into()),
        CallId(call.into()),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redelivery_waiting_at_the_position_door_is_dropped_when_the_session_is_revoked() {
    let r = rig_with(|_| {}, |_| {}, planned(5_000)).await;
    record(&r, 4, &[("toolu_a", "echo_say", "{}")]);
    // First request holds the slot while the tool runs.
    let gate = r.runner.hold_next();
    let first = send_later(&r, &call("toolu_a", "echo_say", "{}"));
    let started = Instant::now();
    while r.runner.jobs().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "tool execution did not start"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Redelivery queues at the position slot.
    let second = send_later(&r, &call("toolu_a", "echo_say", "{}"));
    let started = Instant::now();
    while r.front.shared.slots.waiting(&position("cli-4-0")) != 1 {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waiting redelivery did not reach the slot"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    r.sessions.revoke(&r.token);
    // Waiting redelivery exits immediately even while the first call is still running.
    let (reply, took) = second.await.unwrap();
    assert!(reply.body.contains("stale"), "{}", reply.body);
    assert!(
        took < Duration::from_secs(2),
        "should not wait for the first request to finish: {took:?}"
    );
    assert_eq!(r.runner.jobs().len(), 1, "waiting request must not execute");
    gate.add_permits(1);
    let (reply, _) = first.await.unwrap();
    assert!(reply.body.contains("stale"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 1);
}
