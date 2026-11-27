//! Rate limiting and input-echo protection tests (real listener).
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
        "pre-admission rejection stays SDK-retryable"
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
    rig.api.start_as(answer("finished reply"));
    let body = ask("assistant", "q");
    let first = chat(&rig, BUILD_KEY, Some("again1"), &body).await;
    assert_eq!(first.status, 200, "{}", first.body);
    // After exhausting 1 req/min, new keys get 429 while replaying an accepted key still returns its result.
    let fresh = chat(&rig, BUILD_KEY, Some("again2"), &body).await;
    assert_eq!(fresh.status, 429, "{}", fresh.body);
    assert!(fresh.header("retry-after").is_some());
    let again = chat(&rig, BUILD_KEY, Some("again1"), &body).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json(), first.json());
    assert_eq!(rig.api.runs(), 1);
    let other = chat(
        &rig,
        BUILD_KEY,
        Some("again1"),
        &ask("assistant", "different question"),
    )
    .await;
    assert_eq!(
        (other.status, other.error().1.as_str()),
        (400, "idempotency_key_conflict")
    );
    // Keyless requests skip ledger lookup and fail with 429 immediately.
    let looked = rig.api.lookups();
    let keyless = chat(&rig, BUILD_KEY, None, &body).await;
    assert_eq!(keyless.status, 429, "{}", keyless.body);
    assert_eq!(
        rig.api.lookups(),
        looked,
        "must not query ledger for server-generated key"
    );
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
    assert_eq!(again.json()["choices"][0]["message"]["content"], RUNNING);
    rig.api
        .set_state("wip1", bot_core::api::ApiState::NeedsHuman);
    let waiting = chat(&rig, BUILD_KEY, Some("wip1"), &body).await;
    assert_eq!(waiting.json()["choices"][0]["message"]["content"], PENDING);
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
    // Replay lookups have their own budget (10x principal limit = 10/min); exceeding it returns 422 unknown rather than retryable 429.
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
    // `lim-b` spent one lookup, leaving 9 successful replays out of 10/min.
    assert_eq!((ok, unknown), (9, 5), "replays limited to 10/min");
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
            "field name echoed in response: {}",
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
    rig.api.start_as(answer("finished reply"));
    let body = ask("assistant", "q");
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("done1"), &body).await.status,
        200
    );
    // Confirmed absent in ledger -> pre-admission 429 (retryable).
    let none = chat(&rig, BUILD_KEY, Some("fresh1"), &body).await;
    assert_eq!(none.status, 429, "{}", none.body);
    assert_eq!(none.header("x-should-retry"), None);
    // Ledger error -> outcome unknown (422, x-should-retry: false).
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
    // Lookup timeout -> 422 within deadline.
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
    assert_eq!(
        rig.api.runs(),
        1,
        "must not start a new run when outcome is unknown"
    );
}
