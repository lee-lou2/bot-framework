//! End-to-end OpenAI-compatible HTTP gateway verification with pinned official SDKs
//! (Python `openai==2.29.0`, Node `openai@6.29.0`) through a schema-validating wire proxy
//! against real `serve`, real PG, real `claude` CLI, and a fake model upstream.
//!
//! Setup: `scripts/openai-sdk-setup.sh`
//! Run: `BOT_OPENAI_CANARY=required BOT_CLAUDE_CANARY=required BOT_SRT_CANARY=required BOT_OPENAI_SDK_DIR=... BOT_TEST_DATABASE_URL=... cargo test -p bot-cli --test openai_compat -- --test-threads=1`
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use bot_store::Config;
use claude_support::*;
use serde_json::json;

/// Runs one suite profile (`main` or `tight`) from `openai_sdk/expected.json`, checking proxy
/// schema compliance, consumed scripted turns, zero unapproved tool runs, and zero stored secrets.
async fn run_suite(fixture: &Fixture, sdk: &Sdk, profile: &str) {
    let proxy = sdk.proxy(fixture.port).await;
    let env = sdk.env(fixture, &proxy);
    let (python, node) = sdk.suite(profile);
    assert!(
        !python.is_empty() && !node.is_empty(),
        "{profile}: expected test file list is empty"
    );
    let mut failures = Vec::new();
    let mut ran = 0;
    for file in &python {
        let out = sdk.python(&env, file).await;
        ran += out.ran;
        eprintln!(
            "python {file}: {} {}",
            out.ran,
            if out.ok { "passed" } else { "failed" }
        );
        if !out.ok {
            failures.push(format!("python {file}\n{}", out.log));
        }
    }
    for file in &node {
        let out = sdk.node(&env, file).await;
        ran += out.ran;
        eprintln!(
            "node {file}: {} {}",
            out.ran,
            if out.ok { "passed" } else { "failed" }
        );
        if !out.ok {
            failures.push(format!("node {file}\n{}", out.log));
        }
    }
    let wire = sdk.check_proxy(&env, &proxy, profile).await;
    if !wire.ok {
        failures.push(format!("proxy\n{}", wire.log));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n=====\n"));
    eprintln!("{profile}: {ran} official SDK tests passed");
    let stats = fixture.control.stats().await;
    assert_eq!(
        stats["queued"], 0,
        "scripted model turns were left unconsumed: {stats}"
    );
    assert_eq!(stats["tool_runs"], 0, "tool ran without approval: {stats}");
    assert_eq!(stats["active"], 0, "unfinished runs remain: {stats}");
    assert_eq!(
        fixture.stored_secrets(),
        vec![],
        "plaintext secret value leaked into store"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn official_sdks_run_the_full_scenario_against_the_real_gateway() {
    let one_attempt = Config {
        max_attempts: 1,
        ..Config::default()
    };
    let Some((fixture, sdk)) = Fixture::start(Shape::Main, Some(one_attempt)).await else {
        return;
    };
    run_suite(&fixture, &sdk, "main").await;
    fixture.rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn official_sdks_see_the_wait_deadline_the_saturation_limit_and_the_body_limit_of_a_tight_entrance()
 {
    let one_attempt = Config {
        max_attempts: 1,
        ..Config::default()
    };
    let Some((fixture, sdk)) = Fixture::start(Shape::Tight, Some(one_attempt)).await else {
        return;
    };
    run_suite(&fixture, &sdk, "tight").await;
    fixture.rig.stop().await;
}

const ROTATE_BODY: &str =
    r#"{"model":"assistant","messages":[{"role":"user","content":"rotation e2e"}]}"#;

fn models(fixture: &Fixture, key: &str) -> impl std::future::Future<Output = Reply> {
    fixture.request("GET", "/v1/models", key, None, vec![])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_rotation_and_revocation_take_effect_on_the_next_request_and_never_lose_an_accepted_run()
 {
    let one_attempt = Config {
        max_attempts: 1,
        ..Config::default()
    };
    let Some((fixture, _sdk)) = Fixture::start(Shape::Main, Some(one_attempt)).await else {
        return;
    };
    let keys = &fixture.keys;
    let next = "sk-e2e-rotated-next-0f1e2d3c4b5a69788796a5b4c3d2e1f0";
    let again = "sk-e2e-rotated-again-0f1e2d3c4b5a69788796a5b4c3d2e1f0";
    let status = |reply: &Reply| reply.status;
    assert_eq!(
        (
            status(&models(&fixture, &keys.build).await),
            status(&models(&fixture, &keys.rotated).await)
        ),
        (200, 200)
    );
    // Rotation takes effect immediately without caching stale keys.
    fixture.secrets.set("rotated-api-key", next);
    assert_eq!(status(&models(&fixture, next).await), 200);
    let old = models(&fixture, &keys.rotated).await;
    assert_eq!(old.status, 401);
    assert!(
        !old.body.contains(&keys.rotated) && !old.body.contains(next),
        "error response must never echo keys"
    );
    assert_eq!(status(&models(&fixture, &keys.build).await), 200);
    // Removing one slot disables only that slot.
    fixture.secrets.remove("rotated-api-key");
    assert_eq!(
        (
            status(&models(&fixture, next).await),
            status(&models(&fixture, &keys.build).await)
        ),
        (401, 200)
    );
    // A value matching two slots is ambiguous and rejected.
    fixture.secrets.set("rotated-api-key", &keys.build);
    assert_eq!(
        status(&models(&fixture, &keys.build).await),
        401,
        "ambiguous value must not authenticate"
    );
    assert_eq!(
        status(&models(&fixture, &keys.ops).await),
        200,
        "unrelated key must remain valid"
    );
    fixture.secrets.set("rotated-api-key", again);
    assert_eq!(status(&models(&fixture, &keys.build).await), 200);

    // An accepted run stays queryable via another slot of the same subject; removing all slots returns 401 without creating a new run.
    fixture
        .control
        .script(&json!({"text": "rotation answer", "delay_ms": 6000}));
    let before = fixture.control.stats().await;
    let id = vec![("Idempotency-Key", "rotation-run-1".to_owned())];
    send_and_hang_up(
        &fixture.origin(),
        "/v1/chat/completions",
        &keys.build,
        ROTATE_BODY,
        &[("Idempotency-Key", "rotation-run-1")],
    );
    fixture.secrets.remove("build-api-key");
    let by_other_slot = fixture
        .request(
            "POST",
            "/v1/chat/completions",
            again,
            Some(ROTATE_BODY),
            id.clone(),
        )
        .await;
    assert_eq!(
        by_other_slot.status, 200,
        "another slot of the same subject must query the run: {}",
        by_other_slot.body
    );
    fixture.secrets.remove("rotated-api-key");
    let nobody = fixture
        .request(
            "POST",
            "/v1/chat/completions",
            &keys.build,
            Some(ROTATE_BODY),
            id.clone(),
        )
        .await;
    assert_eq!(nobody.status, 401, "{}", nobody.body);
    fixture.secrets.set("build-api-key", &keys.build);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let reply = fixture
            .request(
                "POST",
                "/v1/chat/completions",
                &keys.build,
                Some(ROTATE_BODY),
                id.clone(),
            )
            .await;
        if reply.status == 200 && reply.body.contains("rotation answer") {
            assert_eq!(reply.header("idempotency-key"), Some("rotation-run-1"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "did not receive final answer: {} {}",
            reply.status,
            reply.body
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let after = fixture.control.stats().await;
    assert_eq!(
        after["runs"].as_u64().unwrap() - before["runs"].as_u64().unwrap(),
        1,
        "polling must not create extra runs: {after}"
    );
    assert_eq!(after["queued"], 0, "{after}");
    for value in [
        &keys.build,
        &keys.rotated,
        &keys.ops,
        &keys.guest,
        next,
        again,
    ] {
        assert_eq!(
            fixture.tables_holding(value),
            Vec::<String>::new(),
            "key value leaked into store"
        );
    }
    fixture.rig.stop().await;
}

const PROFILE_WITH_APPROVER: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  api: [run, calendar.read, calendar.delete]
  owner: [run, calendar.read]
members:
  - {identity: "local:owner", roles: [owner]}
  - {identity: "http:openai/build", roles: [api]}
  - {identity: "http:openai/ops", roles: [api]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
  - {id: assistant, adapter: http, endpoint: openai, model: assistant, agent: main, approvers: {"http:openai/ops": {principal: "local:owner"}}}
  - {id: secret, adapter: http, endpoint: openai, model: secret-agent, agent: main/secret}
secrets: []
policy:
  scopes: [run, calendar.read, calendar.delete]
  agents:
    main/secret: [calendar.read]
  approve: [calendar.delete]
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_last_key_slot_of_one_subject_keeps_the_activation_and_only_that_key_loses_access()
 {
    let one_attempt = Config {
        max_attempts: 1,
        ..Config::default()
    };
    let Some((mut fixture, _sdk)) =
        Fixture::start_with_profile(Shape::Main, Some(one_attempt), PROFILE_WITH_APPROVER).await
    else {
        return;
    };
    let keys = Keys::new();
    for (label, key) in [
        ("build", &keys.build),
        ("rotated", &keys.rotated),
        ("ops", &keys.ops),
    ] {
        assert_eq!(
            models(&fixture, key).await.status,
            200,
            "{label}: pre-deploy"
        );
    }
    let slots = ["build", "rotated", "guest"];
    fixture
        .rig
        .reactivate_host(
            &gateway_with(Shape::Main, fixture.port, &slots),
            PROFILE_WITH_APPROVER,
            &[Role::Gateway, Role::Worker],
        )
        .await;
    assert_eq!(
        models(&fixture, &keys.ops).await.status,
        401,
        "dropped slot must return 401 after redeploy"
    );
    for (label, key) in [("build", &keys.build), ("rotated", &keys.rotated)] {
        assert_eq!(
            models(&fixture, key).await.status,
            200,
            "{label}: remaining subjects must keep access"
        );
    }
    assert_eq!(
        models(&fixture, &keys.guest).await.status,
        403,
        "non-member key must still return 403"
    );
    fixture.rig.stop().await;
}
