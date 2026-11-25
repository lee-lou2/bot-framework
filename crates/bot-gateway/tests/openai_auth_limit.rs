//! API 키 인증 실패의 출처별 한도(실제 리스너): 맞지 않는 키를 계속 보내는 출처는 비밀 보관소를 읽기 전에 429 + Retry-After로 거절되고,
//! 다른 출처와 정상 운영은 영향이 없다. 실제 연결의 피어 주소(`ConnectInfo`)로 센다.
use openai_support::*;
use serde_json::json;
use std::time::Duration;

mod openai_support;

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
    assert!(reads > 0, "맞춰 보려면 보관소를 읽는다");
    // 한도를 넘었다: 이제부터는 키가 맞든 틀리든(정상 키여도) 이 출처의 요청은 보관소를 읽지 않고 429다
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
        "차단된 출처의 요청은 보관소를 읽지 않아요"
    );
    assert!(rig.api.accepts().is_empty(), "접수도 없다");
}

#[tokio::test]
async fn another_source_and_the_webhook_are_not_affected_by_a_blocked_source() {
    let rig = rig_with(dual(3)).await;
    let other = rig.alt.expect("듀얼 스택");
    for _ in 0..3 {
        assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 401);
    }
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 429);
    // 다른 출처(IPv6 루프백)의 정상 키는 그대로 된다
    let fine = models_via(other, BUILD_KEY).await;
    assert_eq!(fine.status, 200, "{}", fine.body);
    // 그 출처의 실패는 그 출처의 예산에서만 센다: 같은 출처에서 둘이 실패해도 아직 막히지 않는다
    for _ in 0..2 {
        assert_eq!(models_via(other, WRONG).await.status, 401);
    }
    assert_eq!(models_via(other, BUILD_KEY).await.status, 200);
    // 막힌 출처의 JWT 웹훅 요청은 OpenAI 키 한도와 무관하다
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
    // 키 없는 요청은 추측이 아니다(첫 요청·헬스 체크·잘못 만든 클라이언트): 아무리 보내도 막히지 않는다
    for _ in 0..10 {
        let reply = send(rig.addr, "GET", "/v1/models", &[], "").await;
        assert_eq!(reply.status, 401);
    }
    // 정상 키는 한도를 쓰지 않는다
    for _ in 0..10 {
        assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
    }
    // 보관소 장애(503)는 키가 틀린 것이 아니다
    rig.secrets.break_it(true);
    for _ in 0..5 {
        assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 503);
    }
    rig.secrets.break_it(false);
    assert_eq!(
        get(&rig, BUILD_KEY, "/v1/models").await.status,
        200,
        "장애 때문에 막히지 않았다"
    );
    // 내밀었는데 맞지 않으면 모양이 틀려도(짧음·다른 스킴) 센다
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
        "세 번의 추측으로 막혔다"
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
    // 없는 경로·틀린 메서드는 키를 보기 전에 가려지므로 그대로 404·405다(키에 대해 아무것도 알리지 않는다)
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
        "차단이 끝났다"
    );
    // 처음부터 다시 센다: 한 번 실패한 것으로는 막히지 않는다
    assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 401);
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
    // 집계 창(400ms)이 지난 옛 실패는 더해지지 않는다
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(get(&rig, WRONG, "/v1/models").await.status, 401);
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn a_key_that_fails_only_at_delivery_is_not_a_guess() {
    // 접수한 뒤 키가 회전돼 내보내기 직전 재확인에서 401이 되는 것은 추측이 아니다: 예산을 쓰지 않는다
    let rig = rig_with(budget(1)).await;
    finish_later(
        &rig,
        "rot1",
        answer("회전 전 결과"),
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("rot1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.secrets.set(
        "build-key",
        concat!("rotated-", "0123456789", "abcdef0123", "456789abc").as_bytes(),
    );
    assert_eq!(pending.await.unwrap().status, 401);
    // 같은 출처의 다른 슬롯 키(회전용)는 그대로 된다(막히지 않았다)
    assert_eq!(get(&rig, BUILD_NEXT_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn a_blocked_source_is_refused_even_without_an_authorization_header() {
    // 키 없는 요청은 실패로 세지 않지만, 이미 차단된 출처의 요청은 머리가 없어도 보관소를 읽기 전에 429다
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
