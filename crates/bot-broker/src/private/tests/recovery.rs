//! 접수 뒤 복구: 시작 전 실패의 리스 반납, 미전송 재점유, 불명 보류, 속도 제한.

use super::*;

/// 게이트가 접수 키·대화·정규화한 인자로 만드는 지문(시험이 원장에 직접 접수할 때 쓴다).
fn fingerprint_of(call: &PrivateCall, canonical: &str) -> [u8; 32] {
    keyed::hmac(
        KEY,
        &[
            call.key.profile.0.as_bytes(),
            call.key.binding.0.as_bytes(),
            call.key.principal.0.as_bytes(),
            call.key.request.0.as_bytes(),
            call.agent.0.as_bytes(),
            call.tool.0.as_bytes(),
            call.conversation.id.as_bytes(),
            call.conversation.thread.as_deref().unwrap_or("").as_bytes(),
            &[u8::from(call.conversation.thread.is_some())],
            canonical.as_bytes(),
        ],
    )
}
use crate::{keyed, testing::*};
use bot_core::{id::ClaimEpoch, state::Risk};

#[tokio::test]
async fn a_delivery_made_while_the_first_is_still_in_flight_is_not_acknowledged() {
    let r = rig(true);
    // 첫 전달이 리스를 점유한 채 시작 전에 멈췄다(워커가 사라졌다): 원장에 예약만 남는다
    use bot_core::ledger::{PrivateLedger, PrivateRequest};
    let first = standard("req-1");
    let sealed = FakeSealer
        .seal(&tool(Risk::Write, &["inbox.write"]), &first.args)
        .unwrap();
    let fingerprint = fingerprint_of(&first, &sealed.canonical);
    let permit = r
        .ledger
        .begin(PrivateRequest {
            key: first.key.clone(),
            fingerprint,
            revision: Revision("r1".into()),
            agent: AgentId("main".into()),
            tool: ToolId("inbox.receive".into()),
            conversation: first.conversation.clone(),
            scopes: ["inbox.write".to_owned()].into(),
        })
        .await
        .unwrap();
    assert!(matches!(permit, bot_core::ledger::PrivatePermit::Start(_)));
    let again = receive(&r, standard("req-1")).await;
    assert!(
        matches!(again, Err(Error::Unavailable)),
        "처리 중인 접수는 ACK하지 않고 다시 시도하게 한다"
    );
    assert!(r.runner.jobs().is_empty(), "호출 0회인데 ACK되면 안 된다");
}

#[tokio::test]
async fn a_failure_before_the_start_record_gives_the_lease_back_and_the_redelivery_runs_once() {
    let r = rig(true);
    // 접수한 뒤 전송 직전에 저장소의 활성 판본이 바뀌었다: 시작을 커밋하지 않고 리스를 반납한다
    let ledger = r.ledger.clone();
    r.ledger
        .set_before_start(move || ledger.set_active_revision("r2"));
    assert!(matches!(
        receive(&r, standard("req-1")).await,
        Err(Error::Stale)
    ));
    assert_eq!(r.ledger.outcomes(), ["retry_before_start"]);
    assert!(
        r.runner.jobs().is_empty(),
        "시작 전 실패에서는 실행기가 불리지 않는다"
    );
    assert!(r.ledger.starts().is_empty());
    // 새 판본을 게시한 뒤 같은 입력이 다시 오면 새 epoch로 이어 가 처리 1회다
    r.catalog.publish(world("r2"));
    let mut again = standard("req-1");
    again.revision = Revision("r2".into());
    receive(&r, again).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1, "재전달 뒤 처리 1회");
    assert_eq!(r.ledger.outcomes(), ["retry_before_start", "completed"]);
}

#[tokio::test]
async fn a_runner_that_confirms_nothing_was_sent_releases_the_claim_for_a_redelivery() {
    let r = rig(true);
    r.runner.script(crate::ports::Ran::NotSent);
    assert!(matches!(
        receive(&r, standard("req-1")).await,
        Err(Error::Unavailable)
    ));
    assert_eq!(r.ledger.outcomes(), ["not_sent"]);
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 2);
    assert_eq!(r.ledger.outcomes(), ["not_sent", "completed"]);
    // 다시 점유할 때마다 새 epoch다
    let claims: Vec<_> = r.runner.jobs().iter().map(|j| j.attempt.claim).collect();
    assert_eq!(claims, [Some(ClaimEpoch(1)), Some(ClaimEpoch(2))]);
}

#[tokio::test]
async fn an_unknown_result_is_held_for_good_and_never_rerun() {
    let r = rig(true);
    r.runner
        .script(crate::ports::Ran::Unknown("response_unknown"));
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(r.ledger.outcomes(), ["held"]);
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(
        r.runner.jobs().len(),
        1,
        "보류는 원문으로 다시 실행하지 않는다"
    );
    // 실행기 자체의 실패도 보류다
    let r = rig(true);
    r.runner.fail_next();
    receive(&r, standard("req-2")).await.unwrap();
    assert_eq!(r.ledger.outcomes(), ["held"]);
}

#[tokio::test]
async fn a_started_claim_that_loses_its_lease_becomes_a_durable_hold_without_a_redelivery() {
    let r = rig(true);
    use bot_core::ledger::{PrivateLedger, PrivateRequest, PrivateStart};
    let call = standard("req-1");
    let sealed = FakeSealer
        .seal(&tool(Risk::Write, &["inbox.write"]), &call.args)
        .unwrap();
    let fingerprint = fingerprint_of(&call, &sealed.canonical);
    let permit = r
        .ledger
        .begin(PrivateRequest {
            key: call.key.clone(),
            fingerprint,
            revision: Revision("r1".into()),
            agent: AgentId("main".into()),
            tool: ToolId("inbox.receive".into()),
            conversation: call.conversation.clone(),
            scopes: ["inbox.write".to_owned()].into(),
        })
        .await
        .unwrap();
    let bot_core::ledger::PrivatePermit::Start(attempt) = permit else {
        panic!()
    };
    r.ledger
        .start(PrivateStart {
            attempt,
            tool: ToolId("inbox.receive".into()),
            fingerprint,
            checked_revision: Revision("r1".into()),
        })
        .await
        .unwrap();
    // 시작한 워커가 사라졌다: 진행 중으로 보이다가 sweep이 보류로 바꾼다
    assert!(matches!(
        receive(&r, standard("req-1")).await,
        Err(Error::Unavailable)
    ));
    r.ledger.expire_started();
    receive(&r, standard("req-1")).await.unwrap();
    assert!(
        r.runner.jobs().is_empty(),
        "시작한 접수는 다시 실행하지 않는다"
    );
}

#[tokio::test]
async fn a_blocked_result_is_held_because_the_effect_is_unknown_and_never_rerun() {
    let r = rig(true);
    r.runner
        .script(crate::ports::Ran::Blocked("result_credential"));
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(
        r.ledger.outcomes(),
        ["held"],
        "도구는 이미 실행됐다: 거절로 단정하지 않고 보류한다"
    );
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1);
}

#[tokio::test]
async fn private_calls_are_rate_limited_and_a_refused_one_gives_the_lease_back() {
    let clock = Arc::new(ManualClock::new(1_000));
    let r = rig(true);
    let gate = PrivateGate::new(
        PrivateParts {
            catalog: r.catalog.clone(),
            ledger: r.ledger.clone(),
            secrets: Arc::new(FakeSecrets::new(&[(
                "personal",
                FINGERPRINT_SECRET,
                "profile-intake-key-bytes",
            )])),
            sealer: Arc::new(FakeSealer),
            runner: r.runner.clone(),
            tracer: r.tracer.clone(),
            clock: clock.clone(),
        },
        crate::rate::RateConfig {
            per_minute: 60,
            burst: 1,
            concurrent: 4,
        },
    );
    gate.receive(standard("req-1")).await.unwrap();
    // 두 번째는 속도 제한으로 시작 전에 멈춘다: 반납했으니 시간이 지난 재전달이 이어 간다(새 접수 키는 새 run이 필요하므로 같은 run 재사용을 흉내 낸다)
    r.ledger.set_attempt(Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("private-run-2".into()),
        generation: Generation(1),
        claim: Some(ClaimEpoch(1)),
    });
    assert!(matches!(
        gate.receive(standard("req-2")).await,
        Err(Error::Unavailable)
    ));
    assert_eq!(r.ledger.outcomes(), ["completed", "retry_before_start"]);
    clock.advance(std::time::Duration::from_secs(2));
    gate.receive(standard("req-2")).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 2);
}

#[tokio::test]
async fn the_reply_destination_is_bound_to_the_first_request_including_the_thread() {
    let r = rig(true);
    let mut first = standard("req-1");
    first.conversation.thread = Some("t-1".into());
    r.runner.script(crate::ports::Ran::NotSent);
    assert!(receive(&r, first).await.is_err()); // 반납된 접수
    // 같은 인자·같은 키인데 스레드가 다르거나 없거나 대화 ID가 다르다: 새 목적지로 바꿔치지 못한다
    for (thread, conversation) in [(Some("t-2"), "dm-1"), (None, "dm-1"), (Some("t-1"), "dm-2")] {
        let mut moved = standard("req-1");
        moved.conversation.thread = thread.map(str::to_owned);
        moved.conversation.id = conversation.into();
        assert!(
            matches!(receive(&r, moved).await, Err(Error::Conflict)),
            "{thread:?}/{conversation}"
        );
    }
    assert_eq!(r.runner.jobs().len(), 1);
    // 처음 그대로면 이어 간다
    let mut same = standard("req-1");
    same.conversation.thread = Some("t-1".into());
    receive(&r, same).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 2);
}

#[tokio::test]
async fn a_tool_that_reports_an_error_is_held_and_a_definite_refusal_is_rejected() {
    let fresh_run = |r: &Rig, n: u32| {
        r.ledger.set_attempt(Attempt {
            profile: ProfileId("personal".into()),
            run: RunId(format!("private-run-{n}")),
            generation: Generation(1),
            claim: Some(ClaimEpoch(1)),
        })
    };
    use crate::ports::Ran;
    use bot_core::command::JsonPayload;
    let r = rig(true);
    r.runner.script(Ran::Done(JsonPayload(
        r#"{"content":[{"text":"boom","type":"text"}],"isError":true}"#.into(),
    )));
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(
        r.ledger.outcomes(),
        ["held"],
        "도구가 오류라고 해도 일부 효과가 났을 수 있다"
    );
    // MCP 모양이 아닌 결과는 성공으로 보지 않는다
    r.runner
        .script(Ran::Done(JsonPayload(r#"{"ok":true}"#.into())));
    fresh_run(&r, 2);
    receive(&r, standard("req-2")).await.unwrap();
    assert_eq!(r.ledger.outcomes(), ["held", "held"]);
    // 서버가 실행 전에 확정적으로 거절했다: 외부 효과가 없다
    r.runner.script(Ran::Rejected);
    fresh_run(&r, 3);
    assert!(matches!(
        receive(&r, standard("req-3")).await,
        Err(Error::Denied)
    ));
    assert_eq!(r.ledger.outcomes(), ["held", "held", "rejected"]);
}
