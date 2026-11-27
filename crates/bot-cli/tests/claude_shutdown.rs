//! Verifies graceful worker shutdown during an active `claude` CLI request: after the 8s grace period,
//! the worker terminates the CLI, releases the run to `ready` without spending an attempt, and a new serve finishes it.
//! Run: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-cli --test claude_shutdown`
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use claude_support::*;
use process_support::Schema;
use std::time::Duration;

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

const ROLES: [Role; 3] = [Role::Gateway, Role::Scheduler, Role::Worker];

fn state(rig: &Rig) -> String {
    rig.query("select state from {s}.bot_runs where parent is null")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_signal_during_a_model_request_returns_the_run_without_spending_the_attempt_and_a_new_serve_finishes_it()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let mut rig = Rig::start(schema, claude, PROFILE, &[], &cli_gateway(), &ROLES).await;

    // Trigger stop while the upstream SSE response is still streaming.
    rig.script_slowly(
        final_turn("msg_slow", "Taking a long time"),
        Duration::from_secs(60),
    );
    let _chat = rig.chat_in_background("s1", "Long-running task");
    rig.until("worker did not lease the run", 30, |rig| {
        state(rig) == "leased"
    })
    .await;
    rig.until("model request was not sent", 30, |rig| {
        rig.upstream.requests().len() == 1
    })
    .await;

    let ended = rig.signal_stop().await;
    assert_eq!(ended, Ok(()), "serve exited with error");
    assert_eq!(state(&rig), "ready", "run must return to ready");
    assert_eq!(
        rig.query("select attempts from {s}.bot_runs where parent is null"),
        "0",
        "shutdown release must not spend an attempt"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_audit where action = 'run.release'"),
        "1"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_audit where action = 'run.reclaim'"),
        "0",
        "must be a graceful release, not lease expiry reclaim"
    );

    // A restarted serve picks up and finishes the same run; re-sending the request ID returns its reply.
    rig.script(final_turn("msg_next", "Resumed and replied"));
    rig.restart_same(&ROLES).await;
    rig.until("restarted serve did not finish the run", 90, |rig| {
        state(rig) == "succeeded"
    })
    .await;
    let answer = rig.chat("s1", "Long-running task").await;
    assert!(
        said(&answer).contains("Resumed and replied"),
        "{}",
        said(&answer)
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where parent is null"),
        "1",
        "must remain the same run"
    );
    rig.stop().await;
}
