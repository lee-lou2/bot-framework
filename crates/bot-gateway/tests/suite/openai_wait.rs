//! OpenAI-compatible ingress wait and outcome tests: approval wait, deadlines, failures, limits, commit faults, and shutdown.
use crate::openai_support::*;
use bot_core::api::{ApiState, ApprovalBlock};
use bot_gateway::testing::api::Fault;
use serde_json::json;
use std::time::{Duration, Instant};

#[tokio::test]
async fn an_approval_wait_is_announced_at_once_and_the_same_post_collects_the_answer_later() {
    let rig = rig().await;
    rig.api.start_as(ApiState::NeedsHuman);
    let started = Instant::now();
    let first = chat(
        &rig,
        BUILD_KEY,
        Some("ap1"),
        &ask("assistant", "send the email"),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "must not wait until deadline: {:?}",
        started.elapsed()
    );
    assert_eq!(first.status, 200, "{}", first.body);
    assert_eq!(content(&first), PENDING);
    let choice = &first.json()["choices"][0];
    assert_eq!(choice["finish_reason"], "stop");
    assert!(
        first.json().get("usage").is_none(),
        "fixed notice must not report usage"
    );
    assert_eq!(first.header("idempotency-key"), Some("ap1"));
    rig.api.set_state("ap1", answer("Email sent"));
    let later = chat(
        &rig,
        BUILD_KEY,
        Some("ap1"),
        &ask("assistant", "send the email"),
    )
    .await;
    assert_eq!(content(&later), "Email sent");
    assert_eq!(later.json()["id"], first.json()["id"]);
    assert_eq!(later.json()["created"], first.json()["created"]);
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn a_run_still_working_at_the_deadline_gets_the_fixed_notice_and_is_not_started_twice() {
    let rig = rig_with(tuned(1500, 5)).await;
    let started = Instant::now();
    let first = chat(
        &rig,
        BUILD_KEY,
        Some("slow"),
        &ask("assistant", "long task"),
    )
    .await;
    let waited = started.elapsed();
    assert_eq!(
        (first.status, content(&first).as_str()),
        (200, RUNNING),
        "{}",
        first.body
    );
    // Wait stops `min(2s, wait_ms/4)` = 375ms early to leave budget for final release checks.
    assert!(
        waited >= Duration::from_millis(1000) && waited < Duration::from_millis(1450),
        "{waited:?}"
    );
    rig.api.set_state("slow", ApiState::Held);
    let held = chat(
        &rig,
        BUILD_KEY,
        Some("slow"),
        &ask("assistant", "long task"),
    )
    .await;
    assert_eq!(content(&held), RUNNING);
    rig.api.set_state("slow", answer("finished"));
    let done = chat(
        &rig,
        BUILD_KEY,
        Some("slow"),
        &ask("assistant", "long task"),
    )
    .await;
    assert_eq!(content(&done), "finished");
    assert_eq!(done.json()["id"], first.json()["id"]);
    assert_eq!(rig.api.runs(), 1, "replays must not start a new run");
}

#[tokio::test]
async fn a_run_that_finishes_while_waiting_is_returned_without_waiting_for_the_deadline() {
    let rig = rig().await;
    finish_later(
        &rig,
        "quick",
        answer("finished early"),
        Duration::from_millis(250),
    );
    let started = Instant::now();
    let reply = chat(&rig, BUILD_KEY, Some("quick"), &ask("assistant", "q")).await;
    assert_eq!(content(&reply), "finished early");
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn failed_cancelled_and_empty_runs_are_422_that_the_sdk_must_not_resend() {
    let rig = rig().await;
    for (key, state, code) in [
        (
            "f1",
            ApiState::Failed {
                reason: "model_error".into(),
            },
            "run_failed",
        ),
        ("f2", ApiState::Cancelled, "run_cancelled"),
        ("f3", answer("   \n"), "run_failed"),
    ] {
        rig.api.start_as(state);
        let reply = chat(&rig, BUILD_KEY, Some(key), &ask("assistant", "q")).await;
        assert_eq!(reply.status, 422, "{code}: {}", reply.body);
        assert_eq!(
            reply.error(),
            ("server_error".into(), code.into(), json!(null))
        );
        assert_eq!(reply.header("x-should-retry"), Some("false"));
        assert_eq!(reply.header("idempotency-key"), Some(key));
        assert!(
            !reply.body.contains("model_error"),
            "must not echo internal failure reason"
        );
    }
}

#[tokio::test]
async fn an_approval_that_cannot_be_obtained_is_told_by_the_host_never_by_the_model() {
    let rig = rig().await;
    let mut notices = Vec::new();
    for (at, why) in ApprovalBlock::ALL.into_iter().enumerate() {
        rig.api.start_as(ApiState::ApprovalBlocked(why));
        let reply = chat(
            &rig,
            BUILD_KEY,
            Some(&format!("blk{at}")),
            &ask("assistant", "q"),
        )
        .await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        let text = content(&reply);
        let lower = text.to_ascii_lowercase();
        assert!(lower.contains("not") && lower.contains("approv"), "{text}");
        assert!(
            !text.contains(why.code()),
            "must not expose raw vocabulary code: {text}"
        );
        assert_eq!(reply.json()["choices"][0]["finish_reason"], "stop");
        notices.push(text);
    }
    notices.sort();
    notices.dedup();
    assert_eq!(
        notices.len(),
        ApprovalBlock::ALL.len(),
        "each reason must map to a distinct notice"
    );
}

#[tokio::test]
async fn the_run_limit_is_a_429_for_new_work_only_and_replays_still_work_at_the_limit() {
    let rig = rig_with(tuned(1000, 1)).await;
    let first = chat(&rig, BUILD_KEY, Some("lim1"), &ask("assistant", "one")).await;
    assert_eq!(content(&first), RUNNING);
    let full = chat(&rig, BUILD_KEY, Some("lim2"), &ask("assistant", "two")).await;
    assert_eq!(full.status, 429, "{}", full.body);
    assert_eq!(
        full.error(),
        (
            "rate_limit_error".into(),
            "rate_limit_exceeded".into(),
            json!(null)
        )
    );
    assert_eq!(full.header("retry-after"), Some("1"));
    assert_eq!(
        full.header("x-should-retry"),
        None,
        "pre-admission rejection remains SDK-retryable"
    );
    assert_eq!(rig.api.runs(), 1);
    let replay = chat(&rig, BUILD_KEY, Some("lim1"), &ask("assistant", "one")).await;
    assert_eq!((replay.status, content(&replay).as_str()), (200, RUNNING));
    rig.api.set_state("lim1", answer("done"));
    rig.api.start_as(answer("two done"));
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("lim2"), &ask("assistant", "two"))
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn failures_before_the_commit_are_the_only_ones_the_sdk_may_retry() {
    let rig = rig().await;
    rig.api.start_as(answer("ok"));
    for fault in [Fault::Unavailable, Fault::Stale] {
        rig.api.fault(fault);
        let reply = chat(&rig, BUILD_KEY, Some("pre"), &ask("assistant", "q")).await;
        assert_eq!(reply.status, 503, "{fault:?}: {}", reply.body);
        assert_eq!(
            reply.error(),
            (
                "server_error".into(),
                "service_unavailable".into(),
                json!(null)
            )
        );
        assert_eq!(reply.header("retry-after"), Some("1"));
        assert_eq!(reply.header("x-should-retry"), None);
    }
    assert_eq!(rig.api.runs(), 0);
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("pre"), &ask("assistant", "q"))
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn a_lost_commit_response_is_resolved_by_reading_the_ledger_with_the_same_key() {
    let rig = rig().await;
    rig.api.start_as(answer("recovered"));
    rig.api.fault(Fault::LostAfterCommit);
    let keyed = chat(&rig, BUILD_KEY, Some("lost1"), &ask("assistant", "q")).await;
    assert_eq!(
        (keyed.status, content(&keyed).as_str()),
        (200, "recovered"),
        "{}",
        keyed.body
    );
    rig.api.fault(Fault::LostAfterCommit);
    let keyless = chat(&rig, BUILD_KEY, None, &ask("assistant", "q2")).await;
    assert_eq!(
        (keyless.status, content(&keyless).as_str()),
        (200, "recovered"),
        "{}",
        keyless.body
    );
    assert_eq!(rig.api.runs(), 2);
}

#[tokio::test]
async fn an_outcome_that_cannot_be_confirmed_is_422_and_never_a_made_up_success_or_a_new_run() {
    let rig = rig_with(tuned(1000, 5)).await;
    rig.api.fault(Fault::UnknownNotCommitted);
    let lost = chat(&rig, BUILD_KEY, Some("unk1"), &ask("assistant", "q")).await;
    assert_eq!(lost.status, 422, "{}", lost.body);
    assert_eq!(
        lost.error(),
        (
            "server_error".into(),
            "request_outcome_unknown".into(),
            json!(null)
        )
    );
    assert_eq!(lost.header("x-should-retry"), Some("false"));
    assert_eq!(
        lost.header("idempotency-key"),
        Some("unk1"),
        "must return the applied key for recovery"
    );
    rig.api.fault(Fault::LostAfterCommit);
    rig.api.fail_lookups(true);
    let blind = chat(&rig, BUILD_KEY, Some("unk2"), &ask("assistant", "q")).await;
    assert_eq!(
        (blind.status, blind.error().1.as_str()),
        (422, "request_outcome_unknown")
    );
    assert_eq!(blind.header("x-should-retry"), Some("false"));
    rig.api.fail_lookups(false);
    rig.api.fault(Fault::Hang(Duration::from_secs(5)));
    let started = Instant::now();
    let slow = chat(&rig, BUILD_KEY, Some("unk3"), &ask("assistant", "q")).await;
    assert_eq!(
        (slow.status, slow.error().1.as_str()),
        (422, "request_outcome_unknown"),
        "{}",
        slow.body
    );
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(slow.header("x-should-retry"), Some("false"));
    rig.api.start_as(answer("now succeeded"));
    assert_eq!(
        content(&chat(&rig, BUILD_KEY, Some("unk1"), &ask("assistant", "q")).await),
        "now succeeded"
    );
}

#[tokio::test]
async fn losing_the_ledger_while_waiting_is_an_unknown_outcome_at_the_deadline() {
    let rig = rig_with(tuned(1000, 5)).await;
    let pending = spawn_chat(&rig, BUILD_KEY, Some("blind"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    rig.api.fail_lookups(true);
    let reply = pending.await.unwrap();
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(reply.error().1, "request_outcome_unknown");
    assert_eq!(reply.header("x-should-retry"), Some("false"));
}

#[tokio::test]
async fn shutting_down_stops_the_wait_with_the_fixed_notice_and_leaves_the_work_alone() {
    let rig = rig_with(tuned(3000, 5)).await;
    let pending = spawn_chat(&rig, BUILD_KEY, Some("down1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    rig.stop.cancel();
    let reply = pending.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        (reply.status, content(&reply).as_str()),
        (200, RUNNING),
        "{}",
        reply.body
    );
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn a_lookup_that_is_still_pending_when_the_wait_ends_is_not_an_unknown_outcome() {
    for lookup_ms in [20u64, 60_000] {
        let rig = rig_with(tuned(900, 5)).await;
        rig.api.delay_lookups(Duration::from_millis(lookup_ms));
        let started = Instant::now();
        let reply = chat(&rig, BUILD_KEY, Some("pend1"), &ask("assistant", "q")).await;
        assert_eq!(
            (reply.status, content(&reply).as_str()),
            (200, RUNNING),
            "lookup {lookup_ms}ms: {}",
            reply.body
        );
        assert!(
            started.elapsed() < Duration::from_millis(1000),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(rig.api.runs(), 1);
    }
}

#[tokio::test]
async fn a_run_that_vanishes_from_the_ledger_while_waiting_is_an_unknown_outcome() {
    let rig = rig_with(tuned(2000, 5)).await;
    let pending = spawn_chat(&rig, BUILD_KEY, Some("gone1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    rig.api.forget("gone1");
    let reply = pending.await.unwrap();
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(reply.error().1, "request_outcome_unknown");
    assert_eq!(reply.header("x-should-retry"), Some("false"));
    assert_eq!(reply.header("idempotency-key"), Some("gone1"));
}
