//! HTTP 입구 수락 시험: 인증 → 접수 → 동기/비동기 응답 → 재조회. 실제 TCP 서버에 원시 HTTP/1.1로 붙는다.
use bot_core::state::RunState;
use bot_gateway::{http::token::sign, testing::*};
use serde_json::{Value, json};
use std::time::Duration;

use crate::http_support::*;

#[tokio::test]
async fn replays_http_result() {
    let rig = rig().await;
    // 워커가 150ms 뒤 끝낸 것처럼 결과를 넣는다
    let store = rig.store.clone();
    let finisher = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        store.set_state(
            &key_of("svc", "k1"),
            RunState::Succeeded,
            Some(r#"{"status":201,"body":{"id":"e-1"}}"#),
        );
    });
    let first = post(&rig, "/inbox/agenda", "svc", "k1", json!({"event": "e-1"})).await;
    finisher.await.unwrap();
    assert_eq!(first.status, 201, "{}", first.body);
    assert_eq!(first.json(), json!({"id": "e-1"}));
    // 같은 요청 재조회: 새 접수·도구 호출 없이 같은 상태·본문
    let again = post(&rig, "/inbox/agenda", "svc", "k1", json!({"event": "e-1"})).await;
    assert_eq!(
        (again.status, again.body.as_str()),
        (201, first.body.as_str())
    );
    assert_eq!(rig.store.accepted().len(), 1);
    let accepted = &rig.store.accepted()[0];
    assert_eq!(
        accepted.target,
        r#"tool main agenda.delete {"event":"e-1"}"#
    );
    // 같은 키에 다른 내용은 충돌이고 기존 접수는 그대로다
    let changed = post(&rig, "/inbox/agenda", "svc", "k1", json!({"event": "e-2"})).await;
    assert_eq!(changed.status, 409);
    assert_eq!(rig.store.accepted().len(), 1);
}

#[tokio::test]
async fn accepted_mode_returns_202_then_the_same_result_at_the_status_url() {
    let rig = rig().await;
    let reply = post(&rig, "/inbox/notify", "svc", "n1", json!({"to": "me"})).await;
    assert_eq!(reply.status, 202);
    assert_eq!(reply.header("location"), Some("/inbox/_requests/n1"));
    assert_eq!(reply.json()["request_id"], "n1");

    let pending = get(&rig, "/inbox/_requests/n1", "svc").await;
    assert_eq!(pending.status, 202);
    assert_eq!(pending.json()["state"], "pending");

    rig.store.set_state(
        &key_of("svc", "n1"),
        RunState::Succeeded,
        Some(r#"{"status":200,"body":{"sent":true}}"#),
    );
    let done = get(&rig, "/inbox/_requests/n1", "svc").await;
    assert_eq!((done.status, done.json()), (200, json!({"sent": true})));
    // 여러 번 조회해도 같고 새 접수는 없다
    let again = get(&rig, "/inbox/_requests/n1", "svc").await;
    assert_eq!(again.body, done.body);
    assert_eq!(rig.store.accepted().len(), 1);

    // 다른 주체는 같은 요청 ID를 볼 수 없다(조회 키에 인증 주체가 들어 있다)
    assert_eq!(get(&rig, "/inbox/_requests/n1", "other").await.status, 404);
    assert_eq!(get(&rig, "/inbox/_requests/never", "svc").await.status, 404);
    assert_eq!(
        get(&rig, "/inbox/_requests/bad%20id", "svc").await.status,
        404
    );
}

#[tokio::test]
async fn a_sync_request_that_outlives_its_deadline_turns_into_202_and_can_be_fetched_later() {
    let rig = rig_with(|host| {
        host.http.endpoints.get_mut("inbox").unwrap().sync_wait_ms = Some(100);
    })
    .await;
    let reply = post(
        &rig,
        "/inbox/agenda",
        "svc",
        "slow",
        json!({"event": "e-9"}),
    )
    .await;
    assert_eq!(reply.status, 202);
    assert_eq!(reply.header("location"), Some("/inbox/_requests/slow"));
    rig.store.set_state(
        &key_of("svc", "slow"),
        RunState::Succeeded,
        Some(r#"{"status":404,"body":{"error":"no such event"}}"#),
    );
    let later = get(&rig, "/inbox/_requests/slow", "svc").await;
    assert_eq!(
        (later.status, later.json()),
        (404, json!({"error": "no such event"}))
    );
    // 같은 POST를 다시 보내도 같은 결과(새 호출 없음)
    let replay = post(
        &rig,
        "/inbox/agenda",
        "svc",
        "slow",
        json!({"event": "e-9"}),
    )
    .await;
    assert_eq!((replay.status, replay.body), (404, later.body));
    assert_eq!(rig.store.accepted().len(), 1);
}

#[tokio::test]
async fn terminal_states_and_unexpected_results_map_to_fixed_responses() {
    let rig = rig().await;
    for (request, state, result, want) in [
        ("d1", RunState::Dead, None, 500),
        ("c1", RunState::Cancelled, None, 409),
        // `{status, body}`를 시도했지만 허용되지 않은 상태·깨진 JSON·결과 없음은 502
        (
            "bad1",
            RunState::Succeeded,
            Some(r#"{"status":500,"body":{}}"#),
            502,
        ),
        ("bad2", RunState::Succeeded, Some("not json"), 502),
        ("bad3", RunState::Succeeded, None, 502),
        // 도구 결과 모양이 아닌 값(에이전트의 답 등)은 감싸서 200
        (
            "agent1",
            RunState::Succeeded,
            Some(r#"{"text":"안녕"}"#),
            200,
        ),
    ] {
        let accepted = post(&rig, "/inbox/notify", "svc", request, json!({})).await;
        assert_eq!(accepted.status, 202, "{request}");
        rig.store.set_state(&key_of("svc", request), state, result);
        let reply = get(&rig, &format!("/inbox/_requests/{request}"), "svc").await;
        assert_eq!(reply.status, want, "{request}: {}", reply.body);
        if request == "agent1" {
            assert_eq!(
                reply.json(),
                json!({"request_id": "agent1", "state": "succeeded", "result": {"text": "안녕"}})
            );
        }
    }
    // 동기 제출은 허용되지 않은 상태 코드를 그대로 쓰지 않고, 도구 결과 모양이 아니면 감싸지 않고 502
    for (request, result) in [
        ("sync-bad", r#"{"status":500,"body":{}}"#),
        ("sync-raw", r#"{"text":"hi"}"#),
    ] {
        let store = rig.store.clone();
        let key = key_of("svc", request);
        let finisher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            store.set_state(&key, RunState::Succeeded, Some(result));
        });
        let reply = post(&rig, "/inbox/agenda", "svc", request, json!({"event": "x"})).await;
        finisher.await.unwrap();
        assert_eq!(reply.status, 502, "{request}");
    }
}

#[tokio::test]
async fn authentication_failures_reveal_nothing_and_unknown_endpoints_are_404() {
    let rig = rig().await;
    let good = format!("Bearer {}", token_for("svc"));
    let claims = |patch: Value| {
        let mut base =
            json!({"iss": "https://issuer.example", "aud": "bot", "sub": "svc", "exp": NOW + 3600});
        for (k, v) in patch.as_object().unwrap() {
            base[k] = v.clone();
        }
        format!("Bearer {}", sign(&base, KEY))
    };
    let cases: Vec<(&str, Option<String>)> = vec![
        ("no header", None),
        ("garbage", Some("Bearer not-a-token".into())),
        ("wrong scheme", Some(good.replace("Bearer", "Basic"))),
        (
            "wrong issuer",
            Some(claims(json!({"iss": "https://evil.example"}))),
        ),
        ("wrong audience", Some(claims(json!({"aud": "other"})))),
        ("expired", Some(claims(json!({"exp": NOW - 3600})))),
        ("bad subject", Some(claims(json!({"sub": "a/b"})))),
        (
            "wrong key",
            Some(format!(
                "Bearer {}",
                sign(
                    &json!({"iss": "https://issuer.example", "aud": "bot", "sub": "svc", "exp": NOW + 60}),
                    b"another-key-another-key-another!!"
                )
            )),
        ),
    ];
    for (name, auth) in cases {
        let mut headers = vec![
            ("Idempotency-Key", "k"),
            ("Content-Type", "application/json"),
        ];
        if let Some(auth) = &auth {
            headers.push(("Authorization", auth));
        }
        for path in ["/inbox/notify", "/inbox/no-such-path", "/inbox/_requests/x"] {
            let reply = send(rig.addr, "POST", path, &headers, "{}").await;
            assert_eq!(reply.status, 401, "{name} {path}");
            assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
            assert_eq!(reply.json(), json!({"error": "unauthorized"}), "{name}");
        }
    }
    assert!(rig.store.accepted().is_empty());
    // 등록되지 않은 입구 별칭은 인증 전에 404
    let reply = send(rig.addr, "POST", "/nobody/notify", &[], "{}").await;
    assert_eq!(reply.status, 404);
    assert_eq!(send(rig.addr, "GET", "/", &[], "").await.status, 404);
}

#[tokio::test]
async fn rejections_use_the_documented_status_codes() {
    let rig = rig().await;
    // 멤버가 아닌 주체
    let stranger = post(&rig, "/inbox/notify", "stranger", "s1", json!({})).await;
    assert_eq!(stranger.status, 403);
    // 맞는 트리거가 없는 경로(인증 뒤)
    assert_eq!(
        post(&rig, "/inbox/other", "svc", "p1", json!({}))
            .await
            .status,
        404
    );
    // Idempotency-Key 없음·잘못된 모양
    let auth = format!("Bearer {}", token_for("svc"));
    let no_key = send(
        rig.addr,
        "POST",
        "/inbox/notify",
        &[
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(
        (no_key.status, no_key.json()["error"].clone()),
        (400, json!("idempotency_key_required"))
    );
    assert_eq!(
        post(&rig, "/inbox/notify", "svc", "has space", json!({}))
            .await
            .status,
        400
    );
    // 본문: 잘못된 JSON·잘못된 Content-Type·너무 큼
    let headers = |ct: &'static str| {
        vec![
            ("Authorization", auth.as_str()),
            ("Idempotency-Key", "b1"),
            ("Content-Type", ct),
        ]
    };
    assert_eq!(
        send(
            rig.addr,
            "POST",
            "/inbox/notify",
            &headers("application/json"),
            "{nope"
        )
        .await
        .status,
        400
    );
    assert_eq!(
        send(
            rig.addr,
            "POST",
            "/inbox/notify",
            &headers("text/plain"),
            "hello"
        )
        .await
        .status,
        415
    );
    let big = json!({"x": "y".repeat(2000)}).to_string();
    assert_eq!(
        send(
            rig.addr,
            "POST",
            "/inbox/notify",
            &headers("application/json"),
            &big
        )
        .await
        .status,
        413
    );
    // 허용하지 않는 method
    assert_eq!(
        send(
            rig.addr,
            "HEAD",
            "/inbox/notify",
            &[("Authorization", auth.as_str())],
            ""
        )
        .await
        .status,
        405
    );
    assert_eq!(
        send(
            rig.addr,
            "POST",
            "/inbox/_requests/x",
            &[("Authorization", auth.as_str()), ("Idempotency-Key", "z")],
            ""
        )
        .await
        .status,
        405
    );
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn rate_limited_requests_get_429_with_retry_after() {
    let rig = rig_with(|host| host.limits.per_principal_per_min = Some(3)).await;
    for n in 0..3 {
        let reply = post(&rig, "/inbox/notify", "svc", &format!("r{n}"), json!({})).await;
        assert_eq!(reply.status, 202, "{n}");
    }
    let limited = post(&rig, "/inbox/notify", "svc", "r9", json!({})).await;
    assert_eq!(limited.status, 429);
    let wait: u64 = limited
        .header("retry-after")
        .expect("Retry-After")
        .parse()
        .unwrap();
    assert!(wait >= 1, "{wait}");
    assert_eq!(limited.json()["error"], "rate_limited");
    // 다른 주체의 예산은 그대로, 거절된 요청은 접수되지 않았다
    assert_eq!(
        post(&rig, "/inbox/notify", "other", "o1", json!({}))
            .await
            .status,
        202
    );
    assert_eq!(rig.store.accepted().len(), 4);
    // 속도 제한에 걸렸던 같은 요청은 한도가 풀리면 그대로 접수될 수 있다(아직 접수된 적 없다)
    assert!(rig.store.accepted().iter().all(|a| a.key.request.0 != "r9"));
}

#[tokio::test]
async fn a_store_outage_is_503_and_a_missing_or_short_key_fails_closed() {
    let rig = rig().await;
    rig.store.fail_next(bot_core::port::Error::Unavailable);
    let outage = post(&rig, "/inbox/notify", "svc", "u1", json!({})).await;
    assert_eq!(outage.status, 503);
    assert!(outage.header("retry-after").is_some());
    // 같은 요청은 복구 뒤 접수된다
    assert_eq!(
        post(&rig, "/inbox/notify", "svc", "u1", json!({}))
            .await
            .status,
        202
    );

    // 키가 없거나 너무 짧거나 다른 프로필에만 있으면 어떤 토큰도 받지 않는다
    for secrets in [
        MemorySecrets::new(&[]),
        MemorySecrets::new(&[("personal", "inbox-key", b"short")]),
        MemorySecrets::new(&[("work", "inbox-key", KEY)]),
    ] {
        let rig = rig_full(vec![fixture()], secrets, |_| {}).await;
        assert_eq!(
            post(&rig, "/inbox/notify", "svc", "k", json!({}))
                .await
                .status,
            503
        );
        assert!(rig.store.accepted().is_empty());
    }
}
