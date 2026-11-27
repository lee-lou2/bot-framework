//! HTTP ingress security tests: key cache invalidation on owner/revision change, revocation, and renderer parity.
use bot_core::{profile::Adapter, state::RunState};
use bot_gateway::testing::*;
use serde_json::{Value, json};
use std::time::Duration;

use crate::http_support::*;

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
    assert_eq!(
        post_with(&rig, &token_with(KEY, "svc"), "a1", "/inbox/notify")
            .await
            .status,
        202
    );
    // Switch endpoint owner within the key cache TTL.
    rig.catalog.replace([owner_of("work").build()]);
    let stale = post_with(&rig, &token_with(KEY, "svc"), "a2", "/inbox/notify").await;
    assert_eq!(stale.status, 401, "{}", stale.body);
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

    // A revision change for the same owner also reloads keys per revision.
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
    // Flip owner right after the first route lookup; admission must stay bound to the authenticated table.
    rig.catalog.flip_after(1, [owner_of("work").build()]);
    let reply = post_with(&rig, &token_with(KEY, "svc"), "m1", "/inbox/notify").await;
    assert_eq!(reply.status, 202, "{}", reply.body);
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].key.profile.0, "personal",
        "must commit under the authenticated profile"
    );
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
        Some(r#"{"status":200,"body":{"output":"hidden-result-one"}}"#),
    );
    let before = get_with(&rig, &token, "/inbox/_requests/r1").await;
    assert_eq!(before.status, 200);
    assert!(before.body.contains("hidden-result-one"));
    rig.catalog.replace([without_svc().build()]);
    let after = get_with(&rig, &token, "/inbox/_requests/r1").await;
    assert_eq!(after.status, 403);
    assert!(!after.body.contains("hidden-result-one"), "{}", after.body);
    let replay = post_with(&rig, &token, "r1", "/inbox/notify").await;
    assert_eq!(replay.status, 403);
    assert!(!replay.body.contains("hidden-result-one"));
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
        catalog.replace([without_svc().build()]);
        tokio::time::sleep(Duration::from_millis(100)).await;
        store.set_state(
            &key_of("svc", "w1"),
            RunState::Succeeded,
            Some(r#"{"status":200,"body":{"output":"hidden-result-two"}}"#),
        );
    });
    let reply = post_with(&rig, &token_with(KEY, "svc"), "w1", "/inbox/agenda").await;
    driver.await.unwrap();
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert!(!reply.body.contains("hidden-result-two"));
}

#[tokio::test]
async fn the_first_response_and_the_requery_use_the_same_renderer() {
    let rig = rig_full(vec![owner_of("personal")], both_keys(), |h| {
        h.http.endpoints.get_mut("inbox").unwrap().sync_wait_ms = Some(2000);
    })
    .await;
    let token = token_with(KEY, "svc");
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
    assert_eq!(
        post_with(&rig, &token, "a1", "/inbox/notify").await.status,
        202
    );
    rig.store.set_state(
        &key_of("svc", "a1"),
        RunState::Succeeded,
        Some(r#"{"text":"summarized"}"#),
    );
    let wrapped = get_with(&rig, &token, "/inbox/_requests/a1").await;
    assert_eq!(wrapped.status, 200);
    assert_eq!(wrapped.json()["result"], json!({"text": "summarized"}));
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
    // Admission records the uppercase routed method and sub-path without query params.
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
    let other_path = post_with(&rig, &token, "k1", "/inbox/agenda").await;
    assert_eq!(other_path.status, 409, "{}", other_path.body);
    assert_eq!(rig.store.accepted().len(), 2);
}

/// Variant where `/agenda` uses accepted mode instead of sync.
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
    // Replays retain the response mode stored at the initial commit even if the trigger changes.
    rig.catalog.replace([agenda_as_accepted().build()]);
    let again = post_with(&rig, &token, "m1", "/inbox/agenda").await;
    assert_eq!(
        (again.status, again.body.clone()),
        (201, first.body.clone()),
        "{}",
        again.body
    );
    let status = get_with(&rig, &token, "/inbox/_requests/m1").await;
    assert_eq!((status.status, status.body), (201, first.body));
    // Legacy mode-selecting status paths do not exist.
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
    rig.store.set_state(
        &key_of("svc", "o1"),
        RunState::Succeeded,
        Some(r#"{"text":"summarized"}"#),
    );
    let reply = get_with(&rig, &token, "/inbox/_requests/o1").await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["result"], json!({"text": "summarized"}));
    let replay = post_with(&rig, &token, "o1", "/inbox/notify").await;
    assert_eq!(replay.status, 202);
}
