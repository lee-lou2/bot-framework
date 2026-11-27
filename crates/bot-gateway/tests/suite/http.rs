//! HTTP ingress acceptance tests over raw HTTP/1.1: auth, admission, sync/async replies, and replay.
use bot_core::state::RunState;
use bot_gateway::{http::token::sign, testing::*};
use serde_json::{Value, json};
use std::time::Duration;

use crate::http_support::*;

#[tokio::test]
async fn replays_http_result() {
    let rig = rig().await;
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
    let again = get(&rig, "/inbox/_requests/n1", "svc").await;
    assert_eq!(again.body, done.body);
    assert_eq!(rig.store.accepted().len(), 1);

    // Status lookup key is scoped to the authenticated principal.
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
        // Disallowed status code, malformed JSON, or missing result map to 502.
        (
            "bad1",
            RunState::Succeeded,
            Some(r#"{"status":500,"body":{}}"#),
            502,
        ),
        ("bad2", RunState::Succeeded, Some("not json"), 502),
        ("bad3", RunState::Succeeded, None, 502),
        // Non-tool payloads (e.g. agent replies) are wrapped with 200 in accepted mode.
        (
            "agent1",
            RunState::Succeeded,
            Some(r#"{"text":"hello"}"#),
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
                json!({"request_id": "agent1", "state": "succeeded", "result": {"text": "hello"}})
            );
        }
    }
    // Sync submissions require a valid `{status, body}` envelope; other shapes map to 502.
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
    // Unregistered endpoint aliases return 404 before auth.
    let reply = send(rig.addr, "POST", "/nobody/notify", &[], "{}").await;
    assert_eq!(reply.status, 404);
    assert_eq!(send(rig.addr, "GET", "/", &[], "").await.status, 404);
}

#[tokio::test]
async fn rejections_use_the_documented_status_codes() {
    let rig = rig().await;
    let stranger = post(&rig, "/inbox/notify", "stranger", "s1", json!({})).await;
    assert_eq!(stranger.status, 403);
    assert_eq!(
        post(&rig, "/inbox/other", "svc", "p1", json!({}))
            .await
            .status,
        404
    );
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
    assert_eq!(
        post(&rig, "/inbox/notify", "other", "o1", json!({}))
            .await
            .status,
        202
    );
    assert_eq!(rig.store.accepted().len(), 4);
    assert!(rig.store.accepted().iter().all(|a| a.key.request.0 != "r9"));
}

#[tokio::test]
async fn a_store_outage_is_503_and_a_missing_or_short_key_fails_closed() {
    let rig = rig().await;
    rig.store.fail_next(bot_core::port::Error::Unavailable);
    let outage = post(&rig, "/inbox/notify", "svc", "u1", json!({})).await;
    assert_eq!(outage.status, 503);
    assert!(outage.header("retry-after").is_some());
    assert_eq!(
        post(&rig, "/inbox/notify", "svc", "u1", json!({}))
            .await
            .status,
        202
    );

    // Missing, too-short, or wrong-profile signing keys fail closed with 503.
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
