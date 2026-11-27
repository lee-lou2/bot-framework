//! CLI refusals that never enter the call ledger (e.g. `schedule.create` with an explicit
//! destination from a CLI conversation) persist a refusal marker (`cli-refusal-<slot>`) before
//! returning the refusal token to the model, avoiding `session_unsaved` and consuming 0 attempts.
//!
//! Run: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-cli --test claude_attempts`
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use bot_store::Config;
use claude_support::*;
use process_support::Schema;

const PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run, schedule.create, schedule.list, schedule.cancel]
members:
  - {identity: "local:owner", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
secrets: []
policy:
  scopes: [run, schedule.create, schedule.list, schedule.cancel]
  approve: []
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_refused_before_it_is_recorded_does_not_spend_an_attempt() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let rig = Rig::start_with_store(
        schema,
        claude,
        PROFILE,
        &cli_gateway(),
        &[Role::Gateway, Role::Worker],
        Config {
            max_attempts: 1,
            ..Config::default()
        },
    )
    .await;
    // CLI conversations only reply in place; explicit destinations fail pre-ledger (`destination_unsupported`).
    let destination =
        serde_json::json!({"binding": "desk", "conversation": {"kind": "channel", "id": "C1"}});
    rig.script(tool_turn(
        "msg_a1",
        "toolu_a1",
        "mcp__relay__schedule_create",
        &create_with(destination, &in_secs(3600)),
    ));
    rig.script(final_turn("msg_a2", "That destination is not supported"));
    let chat = rig.chat("a1", "Notify the team channel").await;
    let rows = rig.query(
        "select string_agg(state || ':' || attempts || ':' || coalesce(error, ''), ' / ') from {s}.bot_runs",
    );
    assert!(chat.status.success(), "{} / runs: {rows}", said(&chat));
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'"),
        "1",
        "run must succeed even with max_attempts = 1: {rows}"
    );
    assert_eq!(
        rig.query("select attempts from {s}.bot_runs"),
        "0",
        "pre-ledger refusal must not consume an attempt: {rows}"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_steps where key like 'cli-refusal-%'"),
        "1"
    );
    assert_eq!(
        rig.query("select body from {s}.bot_steps where key like 'cli-refusal-%'"),
        "\"denied\""
    );
    assert_eq!(
        tool_result(&rig.upstream, "toolu_a1").as_deref(),
        Some("denied")
    );
    // Exactly two model requests: tool call turn + final turn after seeing the refusal.
    assert_eq!(rig.upstream.requests().len(), 2, "{rows}");
    for table in ["bot_user_schedules", "bot_approvals", "bot_calls"] {
        assert_eq!(
            rig.query(&format!("select count(*) from {{s}}.{table}")),
            "0",
            "{table}"
        );
    }
    rig.stop().await;
}
