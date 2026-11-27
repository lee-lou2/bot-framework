//! OpenAI 호환 HTTP 입구(실제 리스너): 경로·메서드, API 키 인증(회전·중복·보관소 장애·슬롯 제거), 모델 목록·조회와 가시성, 같은 호스트의 JWT 웹훅 회귀.
use bot_gateway::testing::*;
use serde_json::json;

use crate::openai_support::*;

#[tokio::test]
async fn unknown_paths_and_wrong_methods_get_the_openai_error_document_before_any_key_is_read() {
    let rig = rig().await;
    for (method, path, status, code, allow) in [
        ("GET", "/v1", 404, "not_found", None),
        ("GET", "/v1/", 404, "not_found", None),
        ("GET", "/v1/nope", 404, "not_found", None),
        ("GET", "/v1/models/", 404, "not_found", None),
        ("GET", "/v1/models/a/b", 404, "not_found", None),
        ("POST", "/v1/models", 405, "method_not_allowed", Some("GET")),
        (
            "DELETE",
            "/v1/models/assistant",
            405,
            "method_not_allowed",
            Some("GET"),
        ),
        (
            "GET",
            "/v1/chat/completions",
            405,
            "method_not_allowed",
            Some("POST"),
        ),
        (
            "PUT",
            "/v1/chat/completions",
            405,
            "method_not_allowed",
            Some("POST"),
        ),
    ] {
        let reply = send(rig.addr, method, path, &[], "").await;
        assert_eq!(reply.status, status, "{method} {path}: {}", reply.body);
        let (kind, found, param) = reply.error();
        assert_eq!(kind, "invalid_request_error", "{path}");
        assert_eq!(found, code, "{path}");
        assert!(param.is_null(), "{path}");
        assert_eq!(reply.header("allow"), allow, "{path}");
        assert!(reply.header("x-request-id").is_some(), "{path}");
        assert_eq!(reply.header("x-should-retry"), None, "{path}");
        let body = reply.json();
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty()),
            "{path}"
        );
    }
}

#[tokio::test]
async fn an_openai_alias_is_not_a_webhook_prefix() {
    // OpenAI 입구는 URL에 별칭이 없다: `/openai/...`는 웹훅 입구가 아니므로 웹훅 형식의 404다(인증·모델 목록에 닿지 않는다)
    let rig = rig().await;
    let auth = bearer(BUILD_KEY);
    for path in ["/openai/models", "/openai/v1/models", "/openai/notify"] {
        let reply = send(rig.addr, "GET", path, &[("Authorization", &auth)], "").await;
        assert_eq!(
            (reply.status, reply.json()),
            (404, json!({"error": "not_found"})),
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_missing_or_malformed_key_is_401_with_a_bearer_challenge() {
    let rig = rig().await;
    let cases: Vec<Vec<(&str, String)>> = vec![
        vec![],
        vec![("Authorization", "Bearer".to_owned())],
        vec![("Authorization", format!("Basic {BUILD_KEY}"))],
        vec![("Authorization", BUILD_KEY.to_owned())],
        vec![("Authorization", "Bearer short".to_owned())],
        vec![("Authorization", format!("Bearer {}", "k".repeat(300)))],
        vec![("Authorization", format!("Bearer {BUILD_KEY}-wrong"))],
        // 표준 헤더가 둘이면 어느 쪽도 믿지 않는다(둘 다 유효한 같은 키여도)
        vec![
            ("Authorization", format!("Bearer {BUILD_KEY}")),
            ("Authorization", format!("Bearer {BUILD_KEY}")),
        ],
        vec![
            ("Authorization", format!("Bearer {BUILD_KEY}")),
            ("Authorization", format!("Bearer {OPS_KEY}")),
        ],
        // 웹훅 입구의 JWT·HS256 키는 OpenAI 입구에서 인증되지 않는다(폴백 없음)
        vec![("Authorization", format!("Bearer {}", jwt("build")))],
        vec![(
            "Authorization",
            format!("Bearer {}", std::str::from_utf8(JWT_KEY).unwrap()),
        )],
    ];
    for headers in cases {
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        for path in ["/v1/models", "/v1/models/assistant"] {
            let reply = send(rig.addr, "GET", path, &pairs, "").await;
            assert_eq!(reply.status, 401, "{headers:?} {path}: {}", reply.body);
            let (kind, code, _) = reply.error();
            assert_eq!(
                (kind.as_str(), code.as_str()),
                ("authentication_error", "invalid_api_key")
            );
            assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
            assert!(!reply.body.contains(BUILD_KEY) && !reply.body.contains(OPS_KEY));
        }
        let reply = send(
            rig.addr,
            "POST",
            "/v1/chat/completions",
            &pairs,
            &ask("assistant", "hi").to_string(),
        )
        .await;
        assert_eq!(reply.status, 401, "{headers:?}");
    }
    assert_eq!(rig.api.runs(), 0);
}

#[tokio::test]
async fn the_key_picks_the_subject_and_models_are_listed_sorted_with_one_stable_created() {
    let rig = rig().await;
    for key in [BUILD_KEY, BUILD_NEXT_KEY] {
        let reply = get(&rig, key, "/v1/models").await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(reply.header("content-type"), Some("application/json"));
        assert!(reply.header("x-request-id").is_some());
        assert_eq!(
            reply.json(),
            json!({"object": "list", "data": [
                {"id": "assistant", "object": "model", "created": 1_750_000_000u64, "owned_by": "bot-framework"},
                {"id": "helper", "object": "model", "created": 1_750_000_000u64, "owned_by": "bot-framework"}]})
        );
    }
    let one = get(&rig, BUILD_KEY, "/v1/models/helper").await;
    assert_eq!(one.status, 200);
    assert_eq!(
        one.json(),
        json!({"id": "helper", "object": "model", "created": 1_750_000_000u64, "owned_by": "bot-framework"})
    );
    // created는 현재 판본의 번들을 처음 보관한 시각이다: 물은 쌍은 (프로필, 현재 판본)이다
    assert!(
        rig.bundles
            .asked()
            .contains(&("personal".into(), "personal-r1".into()))
    );
}

#[tokio::test]
async fn a_model_that_cannot_be_used_is_the_same_404_as_one_that_does_not_exist() {
    let rig = rig().await;
    // ops는 멤버지만 run 스코프가 없다: 목록은 비고, 모든 모델이 «없음»이다
    let list = get(&rig, OPS_KEY, "/v1/models").await;
    assert_eq!(
        (list.status, list.json()),
        (200, json!({"object": "list", "data": []}))
    );
    let hidden = get(&rig, OPS_KEY, "/v1/models/assistant").await;
    let missing = get(&rig, OPS_KEY, "/v1/models/nope").await;
    let also_missing = get(&rig, BUILD_KEY, "/v1/models/nope").await;
    for reply in [&hidden, &missing, &also_missing] {
        assert_eq!(reply.status, 404, "{}", reply.body);
        assert_eq!(
            reply.error(),
            (
                "invalid_request_error".into(),
                "model_not_found".into(),
                json!("model")
            )
        );
    }
    assert_eq!(
        hidden.body, missing.body,
        "가린 모델과 없는 모델은 구별되지 않는다"
    );
    let chat_hidden = chat(&rig, OPS_KEY, Some("k1"), &ask("assistant", "hi")).await;
    assert_eq!(chat_hidden.status, 404, "{}", chat_hidden.body);
    assert_eq!(chat_hidden.body, hidden.body);
    assert_eq!(rig.api.runs(), 0);
}

#[tokio::test]
async fn an_agent_ceiling_without_run_hides_the_model_even_from_a_runner() {
    // helper 모델의 에이전트(side)가 run을 갖지 않으면 runner에게도 안 보인다(역할 ∩ 정책 ∩ 에이전트 상한)
    let fixture = fixture().agent("side", &["calendar.read"]);
    let rig = rig_with(Setup {
        profiles: vec![fixture],
        ..Setup::default()
    })
    .await;
    let list = get(&rig, BUILD_KEY, "/v1/models").await;
    assert_eq!(list.status, 200, "{}", list.body);
    let ids: Vec<String> = list.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["assistant"]);
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models/helper").await.status, 404);
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("k"), &ask("helper", "hi"))
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn a_valid_key_of_a_subject_who_is_not_a_member_is_403() {
    let rig = rig_with(Setup {
        tune: Box::new(|host| {
            let endpoint = host.http.openai.get_mut("openai").unwrap();
            endpoint.keys.insert(
                "ghost".into(),
                bot_gateway::host::KeySlot {
                    secret: "ghost-key".into(),
                    subject: "ghost".into(),
                },
            );
        }),
        ..Setup::default()
    })
    .await;
    rig.secrets.set(
        "ghost-key",
        concat!("ghost-key-", "0123456789", "abcdef0123", "456789a").as_bytes(),
    );
    let key = concat!("ghost-key-", "0123456789", "abcdef0123", "456789a");
    for reply in [
        get(&rig, key, "/v1/models").await,
        get(&rig, key, "/v1/models/assistant").await,
        chat(&rig, key, Some("g1"), &ask("assistant", "hi")).await,
        chat(&rig, key, Some("g2"), &ask("nope", "hi")).await,
    ] {
        assert_eq!(reply.status, 403, "{}", reply.body);
        assert_eq!(reply.error().0, "permission_error");
        assert_eq!(reply.error().1, "access_denied");
    }
    assert_eq!(rig.api.runs(), 0);
}

#[tokio::test]
async fn created_is_never_invented() {
    let rig = rig_with(Setup {
        bundles: None,
        ..Setup::default()
    })
    .await;
    for path in ["/v1/models", "/v1/models/assistant"] {
        let reply = get(&rig, BUILD_KEY, path).await;
        assert_eq!(reply.status, 503, "{path}: {}", reply.body);
        assert_eq!(reply.error().1, "service_unavailable");
        assert_eq!(reply.header("retry-after"), Some("1"));
    }
    rig.bundles.set(Some(1_750_000_000_999));
    assert_eq!(
        get(&rig, BUILD_KEY, "/v1/models/assistant").await.json()["created"],
        1_750_000_000u64
    );
    rig.bundles.fail(true);
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 503);
}

#[tokio::test]
async fn rotating_a_key_in_the_store_takes_effect_on_the_next_request() {
    let rig = rig().await;
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
    // 보관소 값을 바꾸면 옛 키는 곧바로 끊기고 새 키가 열린다(인증 캐시가 갱신을 넘어 수명을 늘리지 않는다)
    let rotated = concat!("rotated-", "0123456789", "abcdef0123", "456789abc");
    rig.secrets.set("build-key", rotated.as_bytes());
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 401);
    assert_eq!(get(&rig, rotated, "/v1/models").await.status, 200);
    // 같은 주체의 다른 슬롯은 영향이 없다(회전용 슬롯)
    assert_eq!(get(&rig, BUILD_NEXT_KEY, "/v1/models").await.status, 200);
    // 값이 비었거나 키 모양이 아니면 그 슬롯으로는 아무도 인증하지 못한다
    rig.secrets.set("build-key", b"");
    assert_eq!(get(&rig, rotated, "/v1/models").await.status, 401);
    assert_eq!(get(&rig, BUILD_NEXT_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn a_slot_whose_value_is_gone_from_the_store_stops_only_that_key() {
    let rig = rig().await;
    rig.secrets.remove("build-key");
    // 없는 값은 누구와도 겹칠 수 없다: 그 슬롯의 키만 끊기고 다른 슬롯·주체는 그대로다(보관소 장애와 다르다)
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 401);
    assert_eq!(get(&rig, BUILD_NEXT_KEY, "/v1/models").await.status, 200);
    assert_eq!(get(&rig, OPS_KEY, "/v1/models").await.status, 200);
    rig.secrets.set("build-key", BUILD_KEY.as_bytes());
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn one_key_value_in_two_slots_authenticates_nobody() {
    let rig = rig().await;
    rig.secrets.set("build-key-next", BUILD_KEY.as_bytes());
    for key in [BUILD_KEY, BUILD_NEXT_KEY] {
        assert_eq!(get(&rig, key, "/v1/models").await.status, 401, "{key}");
    }
    // 다른 주체의 키는 영향이 없다
    assert_eq!(get(&rig, OPS_KEY, "/v1/models").await.status, 200);
    rig.secrets.set("build-key-next", BUILD_NEXT_KEY.as_bytes());
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn a_store_failure_closes_authentication_without_telling_a_good_key_from_a_bad_one() {
    let rig = rig().await;
    rig.secrets.break_it(true);
    let good = get(&rig, BUILD_KEY, "/v1/models").await;
    let bad = get(
        &rig,
        concat!("nonsense-", "0123456789", "abcdef0123", "456789abcd"),
        "/v1/models",
    )
    .await;
    for reply in [&good, &bad] {
        assert_eq!(reply.status, 503, "{}", reply.body);
        assert_eq!(reply.error().1, "service_unavailable");
        assert_eq!(reply.header("retry-after"), Some("1"));
    }
    assert_eq!(good.body, bad.body);
    // 키 모양이 틀린 요청은 보관소를 읽기 전에 거절한다
    assert_eq!(get(&rig, "short", "/v1/models").await.status, 401);
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("k"), &ask("assistant", "hi"))
            .await
            .status,
        503
    );
    assert_eq!(rig.api.runs(), 0);
    rig.secrets.break_it(false);
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn a_store_that_cannot_even_open_the_profile_closes_authentication_too() {
    let rig = rig().await;
    rig.secrets.break_handle(true);
    for key in [
        BUILD_KEY,
        concat!("nonsense-", "0123456789", "abcdef0123", "456789abcd"),
    ] {
        let reply = get(&rig, key, "/v1/models").await;
        assert_eq!(reply.status, 503, "{}", reply.body);
        assert_eq!(reply.error().1, "service_unavailable");
    }
    rig.secrets.break_handle(false);
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn removing_the_last_slot_of_one_subject_closes_that_key_only() {
    let rig = rig_with(Setup {
        tune: Box::new(|host| {
            host.http
                .openai
                .get_mut("openai")
                .unwrap()
                .keys
                .remove("ops");
        }),
        ..Setup::default()
    })
    .await;
    // 프로필의 멤버·승인 매핑은 그대로이고 키 슬롯만 없다: 그 키는 401, 남은 주체는 정상
    assert_eq!(get(&rig, OPS_KEY, "/v1/models").await.status, 401);
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn a_stale_process_closes_the_listing_with_503() {
    let rig = rig().await;
    rig.active.set(Some("set-2"));
    let reply = get(&rig, BUILD_KEY, "/v1/models").await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.error().1, "service_unavailable");
    rig.active.set(Some("set-1"));
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn the_jwt_webhook_on_the_same_host_keeps_its_original_wire() {
    let rig = rig().await;
    let token = jwt("svc");
    let auth = bearer(&token);
    let reply = send(
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
    assert_eq!(reply.status, 202, "{}", reply.body);
    assert_eq!(
        reply.json(),
        json!({"request_id": "n1", "state": "pending"})
    );
    assert_eq!(reply.header("location"), Some("/inbox/_requests/n1"));
    assert_eq!(
        reply.header("x-request-id"),
        None,
        "웹훅 응답에 OpenAI 머리가 붙지 않는다"
    );
    // 웹훅의 오류는 옛 문자열 모양 그대로다
    let denied = send(
        rig.addr,
        "POST",
        "/inbox/notify",
        &[("Idempotency-Key", "n2")],
        "{}",
    )
    .await;
    assert_eq!(
        (denied.status, denied.json()),
        (401, json!({"error": "unauthorized"}))
    );
    assert_eq!(denied.header("www-authenticate"), Some("Bearer"));
    let nokey = send(
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
        (nokey.status, nokey.json()),
        (400, json!({"error": "idempotency_key_required"}))
    );
    let unknown = send(rig.addr, "GET", "/nowhere/x", &[], "").await;
    assert_eq!(
        (unknown.status, unknown.json()),
        (404, json!({"error": "not_found"}))
    );
    let status = send(
        rig.addr,
        "GET",
        "/inbox/_requests/n1",
        &[("Authorization", &auth)],
        "",
    )
    .await;
    assert_eq!(status.status, 202);
    // OpenAI 키는 웹훅 입구에서 쓸 수 없다
    let wrong = send(
        rig.addr,
        "POST",
        "/inbox/notify",
        &[
            ("Authorization", &bearer(BUILD_KEY)),
            ("Idempotency-Key", "n3"),
            ("Content-Type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(wrong.status, 401);
}

#[tokio::test]
async fn a_legacy_webhook_alias_v1_still_works_when_no_openai_entrance_exists() {
    let host = r#"{"http": {"endpoints": {"v1": {"issuer": "https://issuer.example", "audience": "bot", "key": "inbox-key"}}}}"#;
    let legacy = profile("personal")
        .binding(
            "legacy",
            bot_core::profile::Adapter::Http {
                endpoint: "v1".into(),
            },
            "main",
        )
        .member("http:v1/svc", &["svc"])
        .role("svc", &["calendar.delete"])
        .policy_scopes(&["calendar.delete"])
        .trigger(
            "main",
            http_trigger(
                "notify",
                "legacy",
                "POST",
                "/notify",
                tool_target("notify.send", "{}"),
            ),
        );
    let rig = rig_host(
        host,
        Setup {
            profiles: vec![legacy],
            ..Setup::default()
        },
    )
    .await;
    let auth = bearer(&jwt("svc"));
    let reply = send(
        rig.addr,
        "POST",
        "/v1/notify",
        &[
            ("Authorization", &auth),
            ("Idempotency-Key", "n1"),
            ("Content-Type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(reply.status, 202, "{}", reply.body);
    assert_eq!(
        reply.json(),
        json!({"request_id": "n1", "state": "pending"})
    );
    // OpenAI 입구가 없으니 /v1 아래는 전부 웹훅의 몫이다(웹훅의 문자열 오류 모양이고 OpenAI 오류 문서가 아니다)
    let models = send(
        rig.addr,
        "GET",
        "/v1/models",
        &[("Authorization", &auth)],
        "",
    )
    .await;
    assert!(models.json()["error"].is_string(), "{}", models.body);
    assert_eq!(models.header("x-request-id"), None);
}

#[tokio::test]
async fn a_value_with_a_trailing_newline_in_the_store_is_the_same_key() {
    // 보관소 파일 끝의 줄바꿈은 키의 일부가 아니다
    let rig = rig().await;
    rig.secrets
        .set("build-key", format!("{BUILD_KEY}\n").as_bytes());
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
    rig.secrets
        .set("build-key", format!("  {BUILD_KEY} \r\n").as_bytes());
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}

#[tokio::test]
async fn an_entrance_nobody_uses_does_not_take_part_in_key_uniqueness() {
    // 활성 프로필이 쓰지 않는 입구의 슬롯이 같은 자격 이름(같은 값)을 가리켜도 쓰는 입구의 키는 그대로 인증된다
    let rig = rig_with(Setup {
        tune: Box::new(|host| {
            host.http.openai.insert(
                "idle".into(),
                bot_gateway::host::OpenAiEndpoint {
                    keys: std::collections::BTreeMap::from([(
                        "copy".to_owned(),
                        bot_gateway::host::KeySlot {
                            secret: "build-key".into(),
                            subject: "build".into(),
                        },
                    )]),
                    wait_ms: None,
                    max_active_runs: None,
                },
            );
        }),
        ..Setup::default()
    })
    .await;
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}
