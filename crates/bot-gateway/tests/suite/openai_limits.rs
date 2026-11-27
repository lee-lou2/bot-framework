//! 속도 제한과 입력 에코(실제 리스너): 제한에 걸려도 같은 키로 이미 접수한 작업의 재조회는 막히지 않고, 오류 응답은 호출자의 필드 이름을 되풀이하지 않는다.
use crate::openai_support::*;
use serde_json::Value;
use std::time::Duration;

#[tokio::test]
async fn the_per_principal_rate_limit_answers_429_with_a_retry_after_and_keeps_the_sdk_retryable() {
    let rig = rig_with(Setup {
        tune: Box::new(|host| host.limits.per_principal_per_min = Some(2)),
        ..Setup::default()
    })
    .await;
    rig.api.start_as(answer("x"));
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("r1"), &ask("assistant", "q"))
            .await
            .status,
        200
    );
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("r2"), &ask("assistant", "q"))
            .await
            .status,
        200
    );
    let limited = chat(&rig, BUILD_KEY, Some("r3"), &ask("assistant", "q")).await;
    assert_eq!(limited.status, 429, "{}", limited.body);
    assert_eq!(
        limited.error(),
        (
            "rate_limit_error".into(),
            "rate_limit_exceeded".into(),
            Value::Null
        )
    );
    let wait: u64 = limited
        .header("retry-after")
        .expect("Retry-After")
        .parse()
        .unwrap();
    assert!((1..=60).contains(&wait), "{wait}");
    assert_eq!(
        limited.header("x-should-retry"),
        None,
        "접수 전 거절이라 SDK가 다시 보내도 된다"
    );
    assert_eq!(rig.api.runs(), 2);
}

#[tokio::test]
async fn a_request_already_accepted_can_be_asked_again_with_the_same_key_even_when_the_rate_limit_is_spent()
 {
    let rig = rig_with(Setup {
        tune: Box::new(|host| host.limits.per_principal_per_min = Some(1)),
        ..Setup::default()
    })
    .await;
    rig.api.start_as(answer("끝난 답"));
    let body = ask("assistant", "q");
    let first = chat(&rig, BUILD_KEY, Some("again1"), &body).await;
    assert_eq!(first.status, 200, "{}", first.body);
    // 분당 1건을 다 썼다: 새 키는 429지만 같은 키·같은 요청의 재조회는 같은 작업의 결과다(새 실행·새 접수 없음)
    let fresh = chat(&rig, BUILD_KEY, Some("again2"), &body).await;
    assert_eq!(fresh.status, 429, "{}", fresh.body);
    assert!(fresh.header("retry-after").is_some());
    let again = chat(&rig, BUILD_KEY, Some("again1"), &body).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json(), first.json());
    assert_eq!(rig.api.runs(), 1);
    // 같은 키로 다른 요청이면 제한이 아니라 충돌이다
    let other = chat(
        &rig,
        BUILD_KEY,
        Some("again1"),
        &ask("assistant", "다른 질문"),
    )
    .await;
    assert_eq!(
        (other.status, other.error().1.as_str()),
        (400, "idempotency_key_conflict")
    );
    // 서버가 만든 키(키 없는 요청)는 이 요청이 처음이라 찾을 것이 없다: 찾아 보지도 않고 제한 그대로
    let looked = rig.api.lookups();
    let keyless = chat(&rig, BUILD_KEY, None, &body).await;
    assert_eq!(keyless.status, 429, "{}", keyless.body);
    assert_eq!(
        rig.api.lookups(),
        looked,
        "서버가 만든 키로는 원장을 조회하지 않아요"
    );
    // 접수한 적 없는 키는 제한 그대로
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("never-seen"), &body)
            .await
            .status,
        429
    );
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn an_unfinished_request_asked_again_under_the_rate_limit_gets_the_progress_notice_not_a_429()
{
    let rig = rig_with(Setup {
        tune: Box::new(|host| {
            host.limits.per_principal_per_min = Some(1);
            host.http.openai.get_mut("openai").unwrap().wait_ms = Some(800);
        }),
        ..Setup::default()
    })
    .await;
    let body = ask("assistant", "q");
    let first = chat(&rig, BUILD_KEY, Some("wip1"), &body).await;
    assert_eq!(first.status, 200, "{}", first.body);
    let again = chat(&rig, BUILD_KEY, Some("wip1"), &body).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(
        again.json()["choices"][0]["message"]["content"],
        "작업을 처리 중입니다. 같은 Idempotency-Key와 같은 요청으로 다시 조회하세요."
    );
    // 진행 중인 일의 승인 대기도 같다
    rig.api
        .set_state("wip1", bot_core::api::ApiState::NeedsHuman);
    let waiting = chat(&rig, BUILD_KEY, Some("wip1"), &body).await;
    assert_eq!(
        waiting.json()["choices"][0]["message"]["content"],
        "승인 대기 중입니다. 같은 Idempotency-Key와 같은 요청으로 다시 조회하세요."
    );
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn the_entrance_limit_does_not_block_asking_again_either_and_replays_are_limited_in_their_own_way()
 {
    let rig = rig_with(Setup {
        tune: Box::new(|host| {
            host.limits.per_entrance_per_min = Some(1);
            host.limits.per_principal_per_min = Some(1);
        }),
        ..Setup::default()
    })
    .await;
    rig.api.start_as(answer("x"));
    let body = ask("assistant", "q");
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("lim-a"), &body).await.status,
        200
    );
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("lim-b"), &body).await.status,
        429
    );
    // 재조회도 무한하지는 않다: 상태 조회의 속도 제한(주체 한도의 10배 = 분당 10건)을 받는다. 한도를 넘으면 이 키로 이미 접수했는지 **확인할 수 없으므로**
    // 다시 보내도 되는 429가 아니라 «확인 불가»(422 + 재전송 금지)다.
    let mut ok = 0;
    let mut unknown = 0;
    for _ in 0..14 {
        let reply = chat(&rig, BUILD_KEY, Some("lim-a"), &body).await;
        match reply.status {
            200 => ok += 1,
            422 => {
                assert_eq!(reply.error().1, "request_outcome_unknown", "{}", reply.body);
                assert_eq!(reply.header("x-should-retry"), Some("false"));
                assert_eq!(reply.header("idempotency-key"), Some("lim-a"));
                unknown += 1;
            }
            other => panic!("{other}: {}", reply.body),
        }
    }
    // 앞서 새 키(`lim-b`)가 제한에 걸리며 한 번 찾아 봤으므로 그 몫을 뺀 9건이다
    assert_eq!((ok, unknown), (9, 5), "재조회는 분당 10건까지");
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn a_field_name_that_looks_like_a_secret_never_comes_back_in_the_response() {
    let rig = rig().await;
    let secret = concat!("sk-live-", "0123456789", "abcdef");
    for body in [
        format!(
            r#"{{"model":"assistant","messages":[{{"role":"user","content":"q"}}],"{secret}":1}}"#
        ),
        format!(
            r#"{{"model":"assistant","messages":[{{"role":"user","content":"q","{secret}":1}}]}}"#
        ),
        format!(
            r#"{{"model":"assistant","messages":[{{"role":"user","content":[{{"type":"text","text":"q","{secret}":1}}]}}]}}"#
        ),
        format!(
            r#"{{"model":"assistant","stream":true,"stream_options":{{"{secret}":true}},"messages":[{{"role":"user","content":"q"}}]}}"#
        ),
    ] {
        let reply = chat_raw(&rig, BUILD_KEY, Some("echo1"), &[], &body).await;
        assert_eq!(
            (reply.status, reply.error().1.as_str()),
            (400, "unsupported_parameter"),
            "{}",
            reply.body
        );
        assert!(
            !reply.body.contains(secret),
            "필드 이름이 응답에 되풀이됐어요: {}",
            reply.body
        );
        assert!(
            reply
                .headers
                .iter()
                .all(|(_, value)| !value.contains(secret))
        );
    }
    assert!(rig.api.accepts().is_empty());
}

#[tokio::test]
async fn a_ledger_that_cannot_answer_a_rate_limited_replay_is_an_unknown_outcome_never_a_retryable_429()
 {
    let rig = rig_with(Setup {
        tune: Box::new(|host| {
            host.limits.per_principal_per_min = Some(1);
            host.http.openai.get_mut("openai").unwrap().wait_ms = Some(900);
        }),
        ..Setup::default()
    })
    .await;
    rig.api.start_as(answer("끝난 답"));
    let body = ask("assistant", "q");
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("done1"), &body).await.status,
        200
    );
    // 확실히 없음(원장이 «없음»): 접수 전의 제한이라 429(SDK가 다시 보내도 된다)
    let none = chat(&rig, BUILD_KEY, Some("fresh1"), &body).await;
    assert_eq!(none.status, 429, "{}", none.body);
    assert_eq!(none.header("x-should-retry"), None);
    // 원장 오류: 이미 접수했을 수 있다 → 422 + 재전송 금지 + 적용한 키
    rig.api.fail_lookups(true);
    let broken = chat(&rig, BUILD_KEY, Some("done1"), &body).await;
    assert_eq!(
        (broken.status, broken.error().1.as_str()),
        (422, "request_outcome_unknown"),
        "{}",
        broken.body
    );
    assert_eq!(broken.header("x-should-retry"), Some("false"));
    assert_eq!(broken.header("idempotency-key"), Some("done1"));
    rig.api.fail_lookups(false);
    // 시간 초과(조회가 멈춤): 기한 안에 422
    rig.api.delay_lookups(Duration::from_secs(60));
    let started = std::time::Instant::now();
    let stuck = chat(&rig, BUILD_KEY, Some("done1"), &body).await;
    assert_eq!(
        (stuck.status, stuck.error().1.as_str()),
        (422, "request_outcome_unknown"),
        "{}",
        stuck.body
    );
    assert_eq!(stuck.header("x-should-retry"), Some("false"));
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(rig.api.runs(), 1, "확인하지 못해도 새 실행은 없다");
}
