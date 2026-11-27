//! Process-level gateway tests: `validate`/`map`, `--role broker` refusal, `chat` idempotency and conflicts, and activation refusal.
use crate::process_support::*;
use std::{
    process::{Command, Stdio},
    time::Duration,
};

#[test]
fn validate_and_map_work_on_the_example_without_any_service() {
    let profiles = examples();
    let envs = [("BOT_PROFILES", profiles.to_str().unwrap())];
    let validate = bot(&envs, &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
    let shown = text(&validate);
    assert!(shown.contains("Validation passed: 1 profile(s)"), "{shown}");
    assert!(
        shown.contains("demo-agenda") && shown.to_ascii_lowercase().contains("revision"),
        "{shown}"
    );
    let map = bot(&envs, &["map", "--json"]);
    assert!(map.status.success(), "{}", text(&map));
    let json: serde_json::Value = serde_json::from_slice(&map.stdout).unwrap();
    assert!(
        json["graph"]["nodes"]
            .as_array()
            .is_some_and(|n| !n.is_empty())
    );
    assert!(!bot(&envs, &["map", "--profile", "nobody"]).status.success());
    assert!(
        !bot(&[], &["validate"]).status.success(),
        "validate must fail without BOT_PROFILES"
    );
    let bad_secrets = bot(
        &[
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_SECRETS", "vault"),
        ],
        &["validate"],
    );
    assert_eq!(bad_secrets.status.code(), Some(2));
}

#[test]
fn a_broker_alone_is_refused_with_an_explanation_and_starts_nothing() {
    let profiles = examples();
    let output = bot(
        &[("BOT_PROFILES", profiles.to_str().unwrap())],
        &["serve", "--role", "broker"],
    );
    assert!(!output.status.success());
    let message = text(&output);
    assert!(
        message.contains("worker") && message.contains("--role worker"),
        "{message}"
    );
    let all = bot(
        &[("BOT_PROFILES", profiles.to_str().unwrap())],
        &["serve", "--role", "all"],
    );
    assert!(!all.status.success());
    assert!(text(&all).contains("BOT_DATABASE_URL"), "{}", text(&all));
}

#[test]
fn chat_without_a_server_says_how_to_start_one() {
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("none.sock");
    let output = bot(
        &[("BOT_SOCKET", socket.to_str().unwrap())],
        &["chat", "hello"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output).contains("bot serve"), "{}", text(&output));
}

#[test]
fn a_gateway_process_accepts_chat_idempotently_and_reports_conflicts() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let profiles = examples();
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let envs = [
        ("BOT_PROFILES", profiles.to_str().unwrap()),
        ("BOT_DATABASE_URL", url.as_str()),
        ("BOT_SOCKET", socket.to_str().unwrap()),
    ];
    let server = Server(
        Command::new(BOT)
            .args(["serve", "--role", "gateway"])
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
            .envs(envs.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn serve"),
    );
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(socket.exists(), "socket did not open");

    let first = bot(
        &envs,
        &[
            "chat",
            "--no-wait",
            "--request-id",
            "e2e-1",
            "Show today's agenda",
        ],
    );
    assert!(first.status.success(), "{}", text(&first));
    assert!(String::from_utf8_lossy(&first.stdout).contains("e2e-1"));
    let again = bot(
        &envs,
        &[
            "chat",
            "--no-wait",
            "--request-id",
            "e2e-1",
            "Show today's agenda",
        ],
    );
    assert!(again.status.success(), "{}", text(&again));
    assert_eq!(schema.count("bot_runs"), "1");
    let conflict = bot(
        &envs,
        &[
            "chat",
            "--no-wait",
            "--request-id",
            "e2e-1",
            "Different text",
        ],
    );
    assert_eq!(conflict.status.code(), Some(1), "{}", text(&conflict));
    assert!(
        text(&conflict).contains("different request"),
        "{}",
        text(&conflict)
    );
    let attach = bot(
        &envs,
        &["chat", "--attach", "--no-wait", "--request-id", "e2e-1"],
    );
    assert!(attach.status.success(), "{}", text(&attach));
    assert_eq!(schema.count("bot_runs"), "1");
    let missing = bot(
        &envs,
        &["chat", "--attach", "--no-wait", "--request-id", "nope"],
    );
    assert_eq!(missing.status.code(), Some(1));
    drop(server);

    let changed = dir.path().join("profiles");
    copy_dir(&profiles, &changed);
    std::fs::write(
        changed.join("personal/agents/main/AGENT.md"),
        "Different instructions",
    )
    .unwrap();
    let refuse = bot(
        &[
            ("BOT_PROFILES", changed.to_str().unwrap()),
            ("BOT_DATABASE_URL", url.as_str()),
            ("BOT_SOCKET", socket.to_str().unwrap()),
        ],
        &["serve", "--role", "gateway"],
    );
    assert!(!refuse.status.success());
    assert!(text(&refuse).contains("--activate"), "{}", text(&refuse));
}
