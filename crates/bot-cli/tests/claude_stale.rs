//! Changing host model upstream destination invalidates stale fronts even when the profile
//! revision is unchanged (bound via the activation authority fingerprint).
//!
//! Run: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-cli --test claude_stale`
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

fn leased(rig: &Rig) -> String {
    rig.query("select count(*) from {s}.bot_runs where state = 'leased'")
}

fn profile_revision(rig: &Rig) -> String {
    rig.query("select revision from {s}.bot_active_profiles where profile = 'personal'")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_started_attempt_sends_no_new_model_request_after_the_host_destination_changed_and_the_new_worker_finishes_it()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let mut rig = Rig::start(
        schema,
        claude,
        PROFILE,
        &[],
        &cli_gateway(),
        &[Role::Gateway, Role::Scheduler, Role::Worker],
    )
    .await;
    // Control run reaches the initial upstream while host config is unchanged.
    rig.script(final_turn("msg_ok", "Hello there"));
    let ok = rig.chat("s0", "Hello").await;
    assert!(ok.status.success(), "{}", said(&ok));
    assert!(said(&ok).contains("Hello there"), "{}", said(&ok));
    assert_eq!(rig.upstream.requests().len(), 1);

    // Pause before launching the model CLI so the attempt is leased before any model request leaves.
    let (active, profile) = (rig.schema.active(), profile_revision(&rig));
    rig.gate.hold();
    rig.script(final_turn("msg_next", "Continued from the new destination"));
    let mut chat = rig.chat_in_background("s1", "Hello again");
    rig.until("worker did not lease the attempt", 30, |rig| {
        leased(rig) == "1"
    })
    .await;

    // Activate a host-only upstream change while keeping the profile revision identical.
    let _moved = rig.move_upstream().await;
    assert_ne!(
        rig.schema.active(),
        active,
        "activation revision must change"
    );
    assert_eq!(
        profile_revision(&rig),
        profile,
        "profile revision must stay unchanged"
    );

    // Release the stale front: it must refuse forwarding and return the run to `ready` without spending an attempt.
    rig.gate.release();
    rig.until("refused attempt did not unwind", 90, |rig| {
        leased(rig) == "0"
    })
    .await;
    assert_eq!(
        rig.upstream.requests().len(),
        1,
        "stale front sent a new model request to the old destination"
    );
    let waiting = "select state || ':' || attempts from {s}.bot_runs where conv_id = 'owner' order by created_ms desc limit 1";
    assert_eq!(
        rig.query(waiting),
        "ready:0",
        "run must return to ready with attempts unchanged for the new worker"
    );

    let worker = rig.start_worker_on_moved_host();
    rig.until("new worker did not finish the run", 90, |rig| {
        rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'") == "2"
    })
    .await;
    let seen = rig.upstream.requests();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].0, "api.main.example:443",
        "control run should hit the initial destination"
    );
    assert_eq!(
        seen[1].0, "api.moved.example:443",
        "handed-over run must hit only the new destination"
    );
    assert_eq!(rig.query(waiting), "succeeded:0");
    // The stale gateway closes with a retryable error; re-attaching on the new gateway returns the cached result.
    tokio::time::timeout(Duration::from_secs(60), chat.wait())
        .await
        .expect("initial bot chat did not exit")
        .unwrap();
    let attached = rig.chat_on_moved_gateway("s1", "Hello again").await;
    assert!(attached.status.success(), "{}", said(&attached));
    assert!(
        said(&attached).contains("Continued from the new destination"),
        "{}",
        said(&attached)
    );
    assert_eq!(
        rig.upstream.requests().len(),
        2,
        "re-attaching must not invoke the model again"
    );
    worker.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_model_stream_is_cut_when_the_host_destination_changes() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let mut rig = Rig::start(
        schema,
        claude,
        PROFILE,
        &[],
        &cli_gateway(),
        &[Role::Gateway, Role::Scheduler, Role::Worker],
    )
    .await;
    // Drip chunks every 2s so the host destination changes mid-stream.
    rig.script_slowly(
        final_turn("msg_slow", "Stream must not reach the end"),
        Duration::from_secs(2),
    );
    let _chat = rig.chat_in_background("s2", "Give a slow reply");
    rig.until("model request did not reach upstream", 30, |rig| {
        rig.upstream.requests().len() == 1
    })
    .await;
    let _moved = rig.move_upstream().await;

    rig.until("cut attempt did not finish", 90, |rig| leased(rig) == "0")
        .await;
    assert_eq!(
        rig.upstream.requests().len(),
        1,
        "stale front forwarded a retry after cutting the stream"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'"),
        "0",
        "cut stream must not finish the run as succeeded"
    );
    assert!(
        !rig.query("select coalesce(string_agg(result, ''), '') from {s}.bot_runs")
            .contains("Stream must not reach the end")
    );
}
