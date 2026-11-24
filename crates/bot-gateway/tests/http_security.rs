//! HTTP 입구 보안 시험: 입구 소유자·판본이 바뀌는 경우의 키 캐시, 요청 처리 도중 소유자 변경, 철회 뒤 응답 본문 미노출,
//! 최초 응답과 재조회의 같은 렌더러.
use bot_core::{profile::Adapter, state::RunState};
use bot_gateway::testing::*;
use serde_json::{Value, json};
use std::time::Duration;

mod http_support;
use http_support::*;

const KEY_B: &[u8] = b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn owner_of(profile_name: &str) -> Fixture {
    let mut notify = http_trigger(
        "notify",
        "inbox",
        "POST",
        "/notify",
        tool_target("notify.send", "{}"),
    );
    notify.order = 1;
    let mut agenda = sync(http_trigger(
        "agenda",
        "inbox",
        "POST",
        "/agenda",
        tool_target("agenda.delete", "{}"),
    ));
    agenda.order = 2;
    profile(profile_name)
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .member("http:inbox/svc", &["svc"])
        .role("svc", &["calendar.delete"])
        .policy_scopes(&["calendar.delete"])
        .trigger("main", notify)
        .trigger("main", agenda)
}

fn both_keys() -> std::sync::Arc<MemorySecrets> {
    MemorySecrets::new(&[("personal", "inbox-key", KEY), ("work", "inbox-key", KEY_B)])
}

fn token_with(key: &[u8], sub: &str) -> String {
    bot_gateway::http::token::sign(
        &json!({"iss": "https://issuer.example", "aud": "bot", "sub": sub, "exp": NOW + 3600}),
        key,
    )
}

async fn post_with(rig: &Rig, token: &str, key: &str, path: &str) -> Reply {
    send(
        rig.addr,
        "POST",
        path,
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Idempotency-Key", key),
            ("Content-Type", "application/json"),
        ],
        "{}",
    )
    .await
}

#[tokio::test]
async fn a_cached_key_never_outlives_a_change_of_the_endpoint_owner() {
    let rig = rig_full(vec![owner_of("personal")], both_keys(), |_| {}).await;
    // 첫 요청이 personal의 키를 기억하게 한다
    assert_eq!(
        post_with(&rig, &token_with(KEY, "svc"), "a1", "/inbox/notify")
            .await
            .status,
        202
    );
    // 입구 소유자가 work로 바뀐다(키 캐시 TTL 안)
    rig.catalog.replace([owner_of("work").build()]);
    // 옛 키로 서명한 토큰은 곧바로 거절된다
    let stale = post_with(&rig, &token_with(KEY, "svc"), "a2", "/inbox/notify").await;
    assert_eq!(stale.status, 401, "{}", stale.body);
    // 새 소유자의 키는 통하고, 접수는 새 소유자 프로필로 기록된다
    assert_eq!(
        post_with(&rig, &token_with(KEY_B, "svc"), "a3", "/inbox/notify")
            .await
            .status,
        202
    );
    let records = rig.store.accepted();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].key.profile.0, "personal");
    assert_eq!(records[1].key.profile.0, "work");

    // 같은 소유자의 판본이 바뀌어도(키가 바뀐 판본) 판본별로 다시 읽는다
    let rig = rig_full(vec![owner_of("personal")], both_keys(), |_| {}).await;
    assert_eq!(
        post_with(&rig, &token_with(KEY, "svc"), "b1", "/inbox/notify")
            .await
            .status,
        202
    );
    rig.catalog
        .replace([owner_of("personal").revision("personal-r2").build()]);
    assert_eq!(
        post_with(&rig, &token_with(KEY, "svc"), "b2", "/inbox/notify")
            .await
            .status,
        202
    );
}

#[tokio::test]
async fn a_request_is_processed_entirely_under_the_owner_it_authenticated_against() {
    let rig = rig_full(vec![owner_of("personal")], both_keys(), |_| {}).await;
    // 요청 처리 도중(첫 표 조회 직후) 입구 소유자가 work로 바뀐다. 인증에 쓴 표로 접수하므로 personal로만 기록된다.
    rig.catalog.flip_after(1, [owner_of("work").build()]);
    let reply = post_with(&rig, &token_with(KEY, "svc"), "m1", "/inbox/notify").await;
    assert_eq!(reply.status, 202, "{}", reply.body);
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].key.profile.0, "personal",
        "인증한 프로필과 다른 프로필로 접수되면 안 돼요"
    );
    // 이미 바뀐 뒤에 같은 주체가 상태를 조회하면 옛 소유자의 요청이라 응답하지 않는다
    let status = get_with(&rig, &token_with(KEY_B, "svc"), "/inbox/_requests/m1").await;
    assert_ne!(status.status, 200);
    assert!(
        !status.body.contains("m1") || status.status == 404 || status.status == 403,
        "{}",
        status.body
    );
}

async fn get_with(rig: &Rig, token: &str, path: &str) -> Reply {
    send(
        rig.addr,
        "GET",
        path,
        &[("Authorization", &format!("Bearer {token}"))],
        "",
    )
    .await
}

fn without_svc() -> Fixture {
    profile("personal")
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .member("http:inbox/someone", &["svc"])
        .role("svc", &["calendar.delete"])
        .policy_scopes(&["calendar.delete"])
}

#[tokio::test]
async fn revoked_members_get_no_status_or_result_body() {
    let rig = rig_full(vec![owner_of("personal")], both_keys(), |_| {}).await;
    let token = token_with(KEY, "svc");
    assert_eq!(
        post_with(&rig, &token, "r1", "/inbox/notify").await.status,
        202
    );
    rig.store.set_state(
        &key_of("svc", "r1"),
        RunState::Succeeded,
        Some(r#"{"status":200,"body":{"secret":"합성-결과-3d9e"}}"#),
    );
    let before = get_with(&rig, &token, "/inbox/_requests/r1").await;
    assert_eq!(before.status, 200);
    assert!(before.body.contains("합성-결과-3d9e"));
    // 멤버에서 철회된 뒤에는 상태도 결과도 나가지 않는다
    rig.catalog.replace([without_svc().build()]);
    let after = get_with(&rig, &token, "/inbox/_requests/r1").await;
    assert_eq!(after.status, 403);
    assert!(!after.body.contains("합성-결과-3d9e"), "{}", after.body);
    // 같은 요청 ID로 다시 보내도(재조회 성격의 POST) 결과가 나가지 않는다
    let replay = post_with(&rig, &token, "r1", "/inbox/notify").await;
    assert_eq!(replay.status, 403);
    assert!(!replay.body.contains("합성-결과-3d9e"));
}

#[tokio::test]
async fn a_revocation_during_the_synchronous_wait_hides_the_result() {
    let rig = rig_full(vec![owner_of("personal")], both_keys(), |h| {
        h.http.endpoints.get_mut("inbox").unwrap().sync_wait_ms = Some(3000);
    })
    .await;
    let (store, catalog) = (rig.store.clone(), rig.catalog.clone());
    let driver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        catalog.replace([without_svc().build()]); // 기다리는 동안 철회
        tokio::time::sleep(Duration::from_millis(100)).await;
        store.set_state(
            &key_of("svc", "w1"),
            RunState::Succeeded,
            Some(r#"{"status":200,"body":{"secret":"합성-결과-7a1c"}}"#),
        );
    });
    let reply = post_with(&rig, &token_with(KEY, "svc"), "w1", "/inbox/agenda").await;
    driver.await.unwrap();
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert!(!reply.body.contains("합성-결과-7a1c"));
}

#[tokio::test]
async fn the_first_response_and_the_requery_use_the_same_renderer() {
    let rig = rig_full(vec![owner_of("personal")], both_keys(), |h| {
        h.http.endpoints.get_mut("inbox").unwrap().sync_wait_ms = Some(2000);
    })
    .await;
    let token = token_with(KEY, "svc");
    // sync: 모양이 깨진 결과는 최초 응답도 재조회도 502(같은 본문)
    for (id, result) in [
        ("s1", r#"{"text":"agent answer"}"#),
        ("s2", r#"{"status":500,"body":{}}"#),
        ("s3", "not json"),
    ] {
        let store = rig.store.clone();
        let key = key_of("svc", id);
        let driver = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            store.set_state(&key, RunState::Succeeded, Some(result));
        });
        let first = post_with(&rig, &token, id, "/inbox/agenda").await;
        driver.await.unwrap();
        assert_eq!(first.status, 502, "{id}");
        let requery = get_with(&rig, &token, &format!("/inbox/_requests/{id}")).await;
        assert_eq!(
            (requery.status, requery.body),
            (first.status, first.body),
            "{id}"
        );
    }
    // sync: 올바른 결과는 둘 다 같은 상태·본문
    let store = rig.store.clone();
    let driver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        store.set_state(
            &key_of("svc", "ok1"),
            RunState::Succeeded,
            Some(r#"{"status":201,"body":{"id":"e-1"}}"#),
        );
    });
    let first = post_with(&rig, &token, "ok1", "/inbox/agenda").await;
    driver.await.unwrap();
    let requery = get_with(&rig, &token, "/inbox/_requests/ok1").await;
    assert_eq!((first.status, first.json()), (201, json!({"id": "e-1"})));
    assert_eq!((requery.status, requery.body), (first.status, first.body));
    // accepted: 도구 결과 모양이 아닌 값(에이전트의 답)은 재조회가 감싸서 준다
    assert_eq!(
        post_with(&rig, &token, "a1", "/inbox/notify").await.status,
        202
    );
    rig.store.set_state(
        &key_of("svc", "a1"),
        RunState::Succeeded,
        Some(r#"{"text":"요약했어요"}"#),
    );
    let wrapped = get_with(&rig, &token, "/inbox/_requests/a1").await;
    assert_eq!(wrapped.status, 200);
    assert_eq!(wrapped.json()["result"], json!({"text": "요약했어요"}));
    let _: Value = wrapped.json();
}

#[tokio::test]
async fn the_routed_method_and_path_are_part_of_the_request_identity() {
    let rig = rig_full(vec![owner_of("personal")], both_keys(), |_| {}).await;
    let token = token_with(KEY, "svc");
    assert_eq!(
        post_with(&rig, &token, "k1", "/inbox/notify").await.status,
        202
    );
    // 접수 기록에는 실제 라우팅에서 비교한 대문자 method와 입구 아래 경로가 그대로 남는다(쿼리 제외)
    let reply = send(
        rig.addr,
        "POST",
        "/inbox/agenda?trace=1",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Idempotency-Key", "k2"),
            ("Content-Type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(reply.status, 202, "{}", reply.body);
    let routes: Vec<_> = rig.store.accepted().into_iter().map(|a| a.http).collect();
    assert_eq!(
        routes,
        [
            Some("POST /notify".to_string()),
            Some("POST /agenda".to_string())
        ]
    );
    // 같은 키를 다른 경로에 쓰면 같은 요청이 아니다(충돌)
    let other_path = post_with(&rig, &token, "k1", "/inbox/agenda").await;
    assert_eq!(other_path.status, 409, "{}", other_path.body);
    assert_eq!(rig.store.accepted().len(), 2);
    // 채팅 입구에서 온 접수에는 HTTP 경로가 없다는 것은 ingress 시험이 확인한다
}

/// `/agenda` 트리거의 응답 방식만 다른 프로필(접수 뒤 프로필이 바뀐 상황을 흉내 낸다)
fn agenda_as_accepted() -> Fixture {
    let mut notify = http_trigger(
        "notify",
        "inbox",
        "POST",
        "/notify",
        tool_target("notify.send", "{}"),
    );
    notify.order = 1;
    let mut agenda = http_trigger(
        "agenda",
        "inbox",
        "POST",
        "/agenda",
        tool_target("agenda.delete", "{}"),
    );
    agenda.order = 2;
    profile("personal")
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .member("http:inbox/svc", &["svc"])
        .role("svc", &["calendar.delete"])
        .policy_scopes(&["calendar.delete"])
        .trigger("main", notify)
        .trigger("main", agenda)
}

#[tokio::test]
async fn only_the_response_mode_stored_at_the_first_commit_is_used() {
    let rig = rig_full(vec![owner_of("personal")], both_keys(), |h| {
        h.http.endpoints.get_mut("inbox").unwrap().sync_wait_ms = Some(2000);
    })
    .await;
    let token = token_with(KEY, "svc");
    // 최초: sync 트리거. 접수 기록에 응답 방식이 sync로 저장된다
    let store = rig.store.clone();
    let key = key_of("svc", "m1");
    let driver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        store.set_state(
            &key,
            RunState::Succeeded,
            Some(r#"{"status":201,"body":{"id":"e-1"}}"#),
        );
    });
    let first = post_with(&rig, &token, "m1", "/inbox/agenda").await;
    driver.await.unwrap();
    assert_eq!(first.status, 201, "{}", first.body);
    assert_eq!(
        rig.store.accepted()[0]
            .route
            .as_ref()
            .map(|r| (r.method.as_str(), r.path.as_str())),
        Some(("POST", "/agenda"))
    );
    // 프로필이 바뀌어 같은 경로가 accepted 트리거가 된 뒤에도, 같은 키의 재전달은 최초 방식(sync)으로 렌더링한다
    rig.catalog.replace([agenda_as_accepted().build()]);
    let again = post_with(&rig, &token, "m1", "/inbox/agenda").await;
    assert_eq!(
        (again.status, again.body.clone()),
        (201, first.body.clone()),
        "{}",
        again.body
    );
    // 재조회도 저장된 방식으로 같은 결과
    let status = get_with(&rig, &token, "/inbox/_requests/m1").await;
    assert_eq!((status.status, status.body), (201, first.body));
    // 호출자가 방식을 고르는 경로는 없다(옛 URL 꼴은 존재하지 않는다)
    assert_eq!(
        get_with(&rig, &token, "/inbox/_requests/sync/m1")
            .await
            .status,
        404
    );
    assert_eq!(
        get_with(&rig, &token, "/inbox/_requests/accepted/m1")
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn rows_without_stored_http_metadata_are_rendered_conservatively_as_accepted() {
    let rig = rig_full(vec![owner_of("personal")], both_keys(), |_| {}).await;
    let token = token_with(KEY, "svc");
    assert_eq!(
        post_with(&rig, &token, "o1", "/inbox/notify").await.status,
        202
    );
    rig.store.forget_route(&key_of("svc", "o1"));
    // 에이전트의 답 모양 결과는 accepted 방식이라 감싸서 200
    rig.store.set_state(
        &key_of("svc", "o1"),
        RunState::Succeeded,
        Some(r#"{"text":"요약했어요"}"#),
    );
    let reply = get_with(&rig, &token, "/inbox/_requests/o1").await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["result"], json!({"text": "요약했어요"}));
    // 옛 행의 재전달도 sync로 기다리지 않고 202다
    let replay = post_with(&rig, &token, "o1", "/inbox/notify").await;
    assert_eq!(replay.status, 202);
}
