//! Per-source API key failure rate limiting (real listener, keyed by peer `ConnectInfo`).
use crate::openai_support::*;
use serde_json::json;
use std::time::Duration;

const WRONG: &str = concat!("wrong-key-", "0123456789", "abcdef0123", "456789xyz");

fn budget(max: usize) -> Setup {
    Setup {
        auth_failures: (max, Duration::from_secs(60), Duration::from_secs(60)),
        ..Setup::default()
    }
}

fn dual(max: usize) -> Setup {
    Setup {
        dual_stack: true,
        ..budget(max)
    }
}

async fn models_via(addr: std::net::SocketAddr, key: &str) -> Resp {
    send(
        addr,
        "GET",
        "/v1/models",
        &[("Authorization", &bearer(key))],
        "",
    )
    .await
}

#[tokio::test]
async fn a_source_that_keeps_sending_wrong_keys_is_blocked_before_the_store_is_read() {
    let rig = rig_with(dual(3)).await;
    for _ in 0..3 {
        assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 401);
    }
    let reads = rig.secrets.reads();
    assert!(reads > 0, "must read secret store to verify key");
    // Once blocked, even valid keys get 429 without touching the secret store.
    for key in [WRONG, BUILD_KEY, OPS_KEY] {
        let blocked = get(&rig, key, "/v1/models").await;
        assert_eq!(blocked.status, 429, "{}", blocked.body);
        assert_eq!(
            blocked.error(),
            (
                "rate_limit_error".into(),
                "rate_limit_exceeded".into(),
                json!(null)
            )
        );
        let wait: u64 = blocked
            .header("retry-after")
            .expect("Retry-After")
            .parse()
            .unwrap();
        assert!((1..=60).contains(&wait), "{wait}");
        assert!(blocked.header("x-request-id").is_some());
        assert_eq!(blocked.header("x-should-retry"), None);
        assert!(!blocked.body.contains(key));
    }
    let chat_blocked = chat(&rig, BUILD_KEY, Some("blocked1"), &ask("assistant", "q")).await;
    assert_eq!(chat_blocked.status, 429, "{}", chat_blocked.body);
    assert_eq!(
        rig.secrets.reads(),
        reads,
        "blocked source must not read secret store"
    );
    assert!(rig.api.accepts().is_empty(), "must not accept requests");
}

#[tokio::test]
async fn another_source_and_the_webhook_are_not_affected_by_a_blocked_source() {
    let rig = rig_with(dual(3)).await;
    let other = rig.alt.expect("dual stack");
    for _ in 0..3 {
        assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 401);
    }
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 429);
    // Other peer (IPv6 loopback) has an independent budget.
    let fine = models_via(other, BUILD_KEY).await;
    assert_eq!(fine.status, 200, "{}", fine.body);
    for _ in 0..2 {
        assert_eq!(models_via(other, WRONG).await.status, 401);
    }
    assert_eq!(models_via(other, BUILD_KEY).await.status, 200);
    // JWT webhook ingress is unaffected by the OpenAI API-key block.
    let auth = bearer(&jwt("svc"));
    let hook = send(
        rig.addr,
        "POST",
        "/inbox/notify",
        &[
            ("Authorization", &auth),
            ("Idempotency-Key", "n1"),
            ("Content-Type", "application/json"),
        ],
        r#"{"to":"me"}"#,
    )
    .await;
    assert_eq!(hook.status, 202, "{}", hook.body);
}

#[tokio::test]
async fn only_keys_that_were_offered_and_wrong_count_as_guesses() {
    let rig = rig_with(budget(3)).await;
    // Requests without credentials are not counted as key guesses.
    for _ in 0..10 {
        let reply = send(rig.addr, "GET", "/v1/models", &[], "").await;
        assert_eq!(reply.status, 401);
    }
    for _ in 0..10 {
        assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
    }
    // Store outages (503) do not consume the guess budget.
    rig.secrets.break_it(true);
    for _ in 0..5 {
        assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 503);
    }
    rig.secrets.break_it(false);
    assert_eq!(
        get(&rig, BUILD_KEY, "/v1/models").await.status,
        200,
        "store outage must not trigger a block"
    );
    // Any presented invalid credential (wrong scheme, short key, wrong key) counts.
    let auth_basic = format!("Basic {WRONG}");
    assert_eq!(
        send(
            rig.addr,
            "GET",
            "/v1/models",
            &[("Authorization", &auth_basic)],
            ""
        )
        .await
        .status,
        401
    );
    assert_eq!(get(&rig, "short", "/v1/models").await.status, 401);
    assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 401);
    assert_eq!(
        get(&rig, BUILD_KEY, "/v1/models").await.status,
        429,
        "blocked after three wrong guesses"
    );
}

#[tokio::test]
async fn wrong_keys_on_chat_count_too_and_unknown_paths_never_reach_the_check() {
    let rig = rig_with(budget(2)).await;
    for n in 0..2 {
        let reply = chat(&rig, WRONG, Some(&format!("c{n}")), &ask("assistant", "q")).await;
        assert_eq!(reply.status, 401, "{}", reply.body);
    }
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("c9"), &ask("assistant", "q"))
            .await
            .status,
        429
    );
    // Unknown paths and unsupported methods return 404/405 before auth.
    assert_eq!(get(&rig, BUILD_KEY, "/v1/nope").await.status, 404);
    let wrong_method = send(
        rig.addr,
        "DELETE",
        "/v1/models",
        &[("Authorization", &bearer(BUILD_KEY))],
        "",
    )
    .await;
    assert_eq!(wrong_method.status, 405);
}

#[tokio::test]
async fn the_block_ends_and_the_budget_starts_over() {
    let rig = rig_with(Setup {
        auth_failures: (2, Duration::from_millis(400), Duration::from_millis(500)),
        ..Setup::default()
    })
    .await;
    for _ in 0..2 {
        assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 401);
    }
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 429);
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert_eq!(
        get(&rig, BUILD_KEY, "/v1/models").await.status,
        200,
        "block expired"
    );
    assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 401);
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
    // Failures older than the 400ms window expire.
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 401);
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn a_key_that_fails_only_at_delivery_is_not_a_guess() {
    // Post-admission key rotation failing at release recheck does not consume the guess budget.
    let rig = rig_with(budget(1)).await;
    finish_later(
        &rig,
        "rot1",
        answer("pre-rotation result"),
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("rot1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.secrets.set(
        "build-key",
        concat!("rotated-", "0123456789", "abcdef0123", "456789abc").as_bytes(),
    );
    assert_eq!(pending.await.unwrap().status, 401);
    assert_eq!(get(&rig, BUILD_NEXT_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn a_blocked_source_is_refused_even_without_an_authorization_header() {
    // Already-blocked sources get 429 before reading secrets even without an Authorization header.
    let rig = rig_with(budget(2)).await;
    for _ in 0..2 {
        assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 401);
    }
    let reads = rig.secrets.reads();
    let bare = send(rig.addr, "GET", "/v1/models", &[], "").await;
    assert_eq!(bare.status, 429, "{}", bare.body);
    assert!(bare.header("retry-after").is_some());
    assert_eq!(rig.secrets.reads(), reads);
}
