//! 회수된 세션으로는 새 실행이 0이다: 계획·위치 문·거절 표식 읽기를 **기다리는 동안** 세션이 회수되면 그 기다림 뒤의 일(도구 실행·위임·표식)은 시작하지 않고
//! 곧바로 `stale`이다. 기다림이 끝나는 순간과 회수가 겹쳐도 같다(`mcp::unless_revoked`). 결과만 `stale`로 가리고 실행은 일어나는 일이 없다.

use super::{
    cli::{OWNER_RUN, call, planned, record},
    delegate::{delegate, family, plan},
    *,
};
use crate::front::mcp::unless_revoked;
use std::time::Instant;
use tokio::{sync::watch, task::JoinHandle};

/// 요청 하나를 보내 두고 응답과 걸린 시간을 나중에 받는다.
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

/// 어디서도 실행이 시작되지 않았다: 도구 실행도, 호출 기록도 없다.
fn nothing_started(r: &Rig, call_id: &str) {
    assert!(r.runner.jobs().is_empty(), "도구가 실행됐다");
    assert!(!r.ledger.has_call(OWNER_RUN, call_id), "호출 기록이 생겼다");
}

// ---- `unless_revoked` 자체 ----

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
        .expect("회수되면 기다림이 곧바로 끝난다")
        .unwrap();
    assert_eq!(ended, None);
}

#[tokio::test]
async fn a_revocation_that_lands_as_the_wait_ends_still_wins() {
    // 기다림이 값을 돌려주는 바로 그 폴링 안에서 회수된다: 회수 신호 갈래는 이미 지나갔으므로 끝난 직후의 확인만이 잡는다
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
    // 끝나지 않는 기다림도 표가 세션을 내려놓으면(보낸 쪽이 사라지면) 곧바로 끝난다
    let (tx, gone) = watch::channel(false);
    let waiting =
        tokio::spawn(async move { unless_revoked(&gone, std::future::pending::<()>()).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(tx);
    let ended = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("세션이 사라지면 기다림이 곧바로 끝난다")
        .unwrap();
    assert_eq!(ended, None);
}

// ---- 호출 경로 ----

#[tokio::test]
async fn revoking_while_the_refusal_marker_is_read_answers_at_once_and_runs_nothing() {
    let r = rig_with(|_| {}, |_| {}, planned(5_000)).await;
    record(&r, 4, &[("toolu_a", "echo_say", "{}")]);
    // 계획은 읽었고 표식을 읽는 중이다(읽기가 느려진다)
    let gate = r.ledger.hold_next_step("cli-refusal-");
    let waiting = send_later(&r, &call("toolu_a", "echo_say", "{}"));
    r.ledger.step_held().await;
    r.sessions.revoke(&r.token);
    let (reply, took) = waiting.await.unwrap();
    assert!(reply.body.contains("stale"), "{}", reply.body);
    assert!(
        took < Duration::from_secs(2),
        "읽기가 끝나길 기다리지 않고 바로 끝난다: {took:?}"
    );
    // 읽기가 뒤늦게 풀려도 이미 회수된 세션으로는 아무것도 실행되지 않는다
    gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    nothing_started(&r, "cli-4-0");
}

#[tokio::test]
async fn a_revocation_that_lands_as_the_marker_read_returns_still_runs_nothing() {
    let r = rig_with(|_| {}, |_| {}, planned(5_000)).await;
    record(&r, 4, &[("toolu_a", "echo_say", "{}")]);
    // 표식 읽기가 «표식 없음»을 돌려주는 바로 그 순간 회수된다(읽기가 끝나는 순간과 겹친다)
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
    assert!(r.delegation.commands().is_empty(), "자식이 만들어졌다");
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
    assert!(r.delegation.commands().is_empty(), "자식이 만들어졌다");
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
    // 먼저 온 요청이 문을 쥔 채 도구를 실행 중이다(실행을 붙잡아 둔다)
    let gate = r.runner.hold_next();
    let first = send_later(&r, &call("toolu_a", "echo_say", "{}"));
    let started = Instant::now();
    while r.runner.jobs().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "실행이 시작되지 않는다"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // 같은 위치의 재전달이 문 앞에서 기다린다
    let second = send_later(&r, &call("toolu_a", "echo_say", "{}"));
    let started = Instant::now();
    while r.front.shared.slots.waiting(&position("cli-4-0")) != 1 {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "문 앞에서 기다리는 요청이 하나가 되지 않는다"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    r.sessions.revoke(&r.token);
    // 문을 쥔 첫 요청이 아직 실행 중이어도 기다리던 요청은 곧바로 끝난다
    let (reply, took) = second.await.unwrap();
    assert!(reply.body.contains("stale"), "{}", reply.body);
    assert!(
        took < Duration::from_secs(2),
        "첫 요청이 끝나길 기다리지 않는다: {took:?}"
    );
    assert_eq!(r.runner.jobs().len(), 1, "기다리던 요청은 실행하지 않았다");
    gate.add_permits(1);
    let (reply, _) = first.await.unwrap();
    assert!(reply.body.contains("stale"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 1);
}
