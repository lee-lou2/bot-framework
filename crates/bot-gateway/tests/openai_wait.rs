//! OpenAI 호환 입구의 대기와 접수 결과(실제 리스너): 승인 대기 즉시 안내, 기한 안내와 재조회, 완료·실패·취소·승인 불가, 실행 수 한도, 접수 전후 결함과 응답 유실, 종료.
use bot_core::api::{ApiState, ApprovalBlock};
use bot_gateway::testing::api::Fault;
use openai_support::*;
use serde_json::json;
use std::time::{Duration, Instant};

mod openai_support;

#[tokio::test]
async fn an_approval_wait_is_announced_at_once_and_the_same_post_collects_the_answer_later() {
    let rig = rig().await;
    rig.api.start_as(ApiState::NeedsHuman);
    let started = Instant::now();
    let first = chat(
        &rig,
        BUILD_KEY,
        Some("ap1"),
        &ask("assistant", "메일 보내줘"),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "기한까지 기다리지 않는다: {:?}",
        started.elapsed()
    );
    assert_eq!(first.status, 200, "{}", first.body);
    assert_eq!(content(&first), PENDING);
    let choice = &first.json()["choices"][0];
    assert_eq!(choice["finish_reason"], "stop");
    assert!(
        first.json().get("usage").is_none(),
        "고정 안내는 측정값이 아니다"
    );
    assert_eq!(first.header("idempotency-key"), Some("ap1"));
    // 승인 뒤 같은 키·같은 요청: 새 작업 없이 같은 작업의 최종 답을 받는다
    rig.api.set_state("ap1", answer("메일을 보냈어요"));
    let later = chat(
        &rig,
        BUILD_KEY,
        Some("ap1"),
        &ask("assistant", "메일 보내줘"),
    )
    .await;
    assert_eq!(content(&later), "메일을 보냈어요");
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
        &ask("assistant", "오래 걸리는 일"),
    )
    .await;
    let waited = started.elapsed();
    assert_eq!(
        (first.status, content(&first).as_str()),
        (200, RUNNING),
        "{}",
        first.body
    );
    // 기한(1.5초)보다 min(2초, wait_ms/4)=375ms 먼저 기다림을 끝내 마지막 확인과 쓰기에 남긴다
    assert!(
        waited >= Duration::from_millis(1000) && waited < Duration::from_millis(1450),
        "{waited:?}"
    );
    // 보류 중인 일도 «승인 대기»가 아니다
    rig.api.set_state("slow", ApiState::Held);
    let held = chat(
        &rig,
        BUILD_KEY,
        Some("slow"),
        &ask("assistant", "오래 걸리는 일"),
    )
    .await;
    assert_eq!(content(&held), RUNNING);
    rig.api.set_state("slow", answer("끝났어요"));
    let done = chat(
        &rig,
        BUILD_KEY,
        Some("slow"),
        &ask("assistant", "오래 걸리는 일"),
    )
    .await;
    assert_eq!(content(&done), "끝났어요");
    assert_eq!(done.json()["id"], first.json()["id"]);
    assert_eq!(rig.api.runs(), 1, "재조회는 새 실행을 만들지 않는다");
}

#[tokio::test]
async fn a_run_that_finishes_while_waiting_is_returned_without_waiting_for_the_deadline() {
    let rig = rig().await;
    finish_later(
        &rig,
        "quick",
        answer("빨리 끝남"),
        Duration::from_millis(250),
    );
    let started = Instant::now();
    let reply = chat(&rig, BUILD_KEY, Some("quick"), &ask("assistant", "q")).await;
    assert_eq!(content(&reply), "빨리 끝남");
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
            "내부 사유를 되풀이하지 않는다"
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
        assert!(text.contains("실행하지 않았습니다"), "{text}");
        assert!(
            !text.contains(why.code()),
            "고정 낱말을 그대로 내보내지 않는다: {text}"
        );
        assert_eq!(reply.json()["choices"][0]["finish_reason"], "stop");
        notices.push(text);
    }
    notices.sort();
    notices.dedup();
    assert_eq!(
        notices.len(),
        ApprovalBlock::ALL.len(),
        "이유마다 다른 안내"
    );
}

#[tokio::test]
async fn the_run_limit_is_a_429_for_new_work_only_and_replays_still_work_at_the_limit() {
    let rig = rig_with(tuned(1000, 1)).await;
    let first = chat(&rig, BUILD_KEY, Some("lim1"), &ask("assistant", "하나")).await;
    assert_eq!(content(&first), RUNNING);
    let full = chat(&rig, BUILD_KEY, Some("lim2"), &ask("assistant", "둘")).await;
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
        "접수 전이므로 SDK가 다시 보내도 된다"
    );
    assert_eq!(rig.api.runs(), 1);
    // 한도가 차 있어도 이미 접수한 키의 재전달은 새 슬롯 없이 붙는다
    let replay = chat(&rig, BUILD_KEY, Some("lim1"), &ask("assistant", "하나")).await;
    assert_eq!((replay.status, content(&replay).as_str()), (200, RUNNING));
    // 끝나면 새 일을 받는다
    rig.api.set_state("lim1", answer("끝"));
    rig.api.start_as(answer("둘 끝"));
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("lim2"), &ask("assistant", "둘"))
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
    // 같은 키로 다시 보내면(SDK 재시도) 이제 접수된다
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
    rig.api.start_as(answer("복구했어요"));
    // 키가 있는 요청과 서버가 키를 만든 요청 모두 같은 키로 조회해 같은 작업을 찾는다(새 실행 없음)
    rig.api.fault(Fault::LostAfterCommit);
    let keyed = chat(&rig, BUILD_KEY, Some("lost1"), &ask("assistant", "q")).await;
    assert_eq!(
        (keyed.status, content(&keyed).as_str()),
        (200, "복구했어요"),
        "{}",
        keyed.body
    );
    rig.api.fault(Fault::LostAfterCommit);
    let keyless = chat(&rig, BUILD_KEY, None, &ask("assistant", "q2")).await;
    assert_eq!(
        (keyless.status, content(&keyless).as_str()),
        (200, "복구했어요"),
        "{}",
        keyless.body
    );
    assert_eq!(rig.api.runs(), 2);
}

#[tokio::test]
async fn an_outcome_that_cannot_be_confirmed_is_422_and_never_a_made_up_success_or_a_new_run() {
    let rig = rig_with(tuned(1000, 5)).await;
    // 커밋하지 못했는데 모른다고 답한 접수: 같은 키 조회에 없다
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
        "복구할 키를 알려 준다"
    );
    // 조회마저 실패
    rig.api.fault(Fault::LostAfterCommit);
    rig.api.fail_lookups(true);
    let blind = chat(&rig, BUILD_KEY, Some("unk2"), &ask("assistant", "q")).await;
    assert_eq!(
        (blind.status, blind.error().1.as_str()),
        (422, "request_outcome_unknown")
    );
    assert_eq!(blind.header("x-should-retry"), Some("false"));
    rig.api.fail_lookups(false);
    // 접수 호출이 기한을 넘기면 커밋 여부를 모른다
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
    // 그 키를 다시 보내면 접수된다(첫 시도는 커밋되지 않았다)
    rig.api.start_as(answer("이제 됐어요"));
    assert_eq!(
        content(&chat(&rig, BUILD_KEY, Some("unk1"), &ask("assistant", "q")).await),
        "이제 됐어요"
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
    // 실제 저장소처럼 조회가 즉시 끝나지 않는다(대기 끝까지 일이 안 끝난다): 기한에는 «처리 중» 안내이고 422가 아니다
    for lookup_ms in [20u64, 60_000] {
        let rig = rig_with(tuned(900, 5)).await;
        rig.api.delay_lookups(Duration::from_millis(lookup_ms));
        let started = Instant::now();
        let reply = chat(&rig, BUILD_KEY, Some("pend1"), &ask("assistant", "q")).await;
        assert_eq!(
            (reply.status, content(&reply).as_str()),
            (200, RUNNING),
            "조회 {lookup_ms}ms: {}",
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
