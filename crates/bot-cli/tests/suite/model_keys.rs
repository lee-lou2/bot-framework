//! Preflight checks for `claude-cli` model service keys and VM config in `bot validate` and worker startup.

use crate::claude_support::*;
use crate::process_support::*;
use bot_broker::testing::FakeSecrets;
use bot_cli::cli::Role;
use std::sync::Arc;

const PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run]
members:
  - {identity: "local:owner", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
secrets: []
policy:
  scopes: [run]
  approve: []
"#;

#[test]
fn validate_fails_without_the_model_service_key_and_names_where_to_put_it() {
    let dir = tempfile::Builder::new()
        .prefix("k")
        .tempdir_in("/tmp")
        .unwrap();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let providers = r#", "providers": {"assistant": {"kind": "claude-cli", "model": "model-large",
                "endpoint": "https://api.example", "secret": "model-key"}}"#;
    let vm = format!(
        r#", "sandbox": {{"apple": {{"image": "img@sha256:{}", "network": "bot-internal"}}}}"#,
        "a".repeat(64)
    );
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(my_uid(), &pkg, "", &format!("{providers}{vm}")),
    )
    .unwrap();
    let bare = dir.path().join("host-bare.json");
    std::fs::write(&bare, host_json(my_uid(), &pkg, "", providers)).unwrap();
    let socket = dir.path().join("g.sock");
    let env_on = |host: &std::path::Path, extra: &[(&str, &str)]| {
        let mut pairs = vec![("BOT_HOST_CONFIG", host.to_str().unwrap())];
        pairs.extend_from_slice(extra);
        env_for(
            &examples(),
            "postgres://unused@localhost/none",
            &socket,
            &pairs,
        )
    };
    let env = |extra: &[(&str, &str)]| env_on(&host, extra);
    let missing = bot(&refs(&env(&[])), &["validate"]);
    assert_eq!(missing.status.code(), Some(1), "{}", text(&missing));
    let message = text(&missing);
    assert!(
        message.contains("model-key") && message.contains("BOT_SECRET__personal__model_key"),
        "{message}"
    );
    assert!(!message.contains("Validation passed"), "{message}");

    let value = "value-for-the-model-key";
    let ok = bot(
        &refs(&env(&[("BOT_SECRET__personal__model_key", value)])),
        &["validate"],
    );
    assert!(ok.status.success(), "{}", text(&ok));
    assert!(text(&ok).contains("Validation passed"), "{}", text(&ok));
    assert!(!text(&ok).contains(value));

    let no_vm = bot(
        &refs(&env_on(
            &bare,
            &[("BOT_SECRET__personal__model_key", value)],
        )),
        &["validate"],
    );
    assert_eq!(no_vm.status.code(), Some(1), "{}", text(&no_vm));
    assert!(text(&no_vm).contains("sandbox.apple"), "{}", text(&no_vm));
    assert!(
        !text(&no_vm).contains("Validation passed"),
        "{}",
        text(&no_vm)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_without_the_model_service_key_stops_before_publishing_anything() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let mut rig = Rig::launch(
        schema,
        "/nonexistent/claude".into(),
        PROFILE,
        &[],
        &cli_gateway(),
        &[Role::Worker],
        Arc::new(FakeSecrets::new(&[])),
        "",
        None,
    )
    .await;
    let error = rig.failure().await;
    let lower = error.to_ascii_lowercase();
    assert!(
        lower.contains("service key") || lower.contains("model key"),
        "{error}"
    );
    assert!(error.contains("model-key"), "{error}");
    assert_eq!(rig.schema.active(), "", "must stop before publishing");
    assert_eq!(
        rig.query("select count(*) from {s}.bot_active_profiles"),
        "0"
    );
}
