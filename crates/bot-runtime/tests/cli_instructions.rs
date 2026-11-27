//! Verifies with the real `claude` CLI (fake upstream), PostgreSQL, and broker front that profile instructions reach the model request's `system` prompt.
//! Because `--bare` does not autoload `AGENTS.md`/`CLAUDE.md`, the runner passes `--append-system-prompt-file`; removing that flag fails this test.
//! Run: `BOT_CLAUDE_BIN=<path> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-runtime --test cli_instructions`

mod support;

use bot_broker::testing::FakeUpstream;
use bot_core::id::*;
use bot_runtime::Outcome;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

const TRUSTED: &str = "TRUSTED-INSTRUCTION-4410 Reply briefly in English";
const QUESTION: &str = "USER-QUESTION-8821 Show today's agenda";

fn texts(blocks: &Value) -> String {
    match blocks {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[tokio::test]
async fn the_profiles_instructions_reach_the_models_system_prompt_and_the_users_words_stay_in_the_user_message()
 {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let mut snapshot = (*profile()).clone();
    snapshot
        .agents
        .get_mut(&AgentId("main".into()))
        .expect("root agent")
        .instructions = TRUSTED.into();
    w.catalog.publish(Arc::new(snapshot));
    let upstream = Arc::new(FakeUpstream::new());
    script(&upstream, final_turn("msg_i1", "Here is the answer"));

    let run = w.accept("req-instructions", QUESTION).await;
    let host = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        LocalSandbox::new(claude),
        &workspace("instructions"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.pg.state_of(&run).await, "succeeded");

    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 1, "expected a single model request");
    let system = texts(&seen[0]["system"]);
    assert!(
        system.contains(TRUSTED),
        "profile instructions missing from system prompt: {system}"
    );
    assert!(system.contains("## Current Time"), "{system}");
    assert!(!system.contains("USER-QUESTION-8821"), "{system}");
    let messages = seen[0]["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["role"], "user");
    assert!(texts(&messages[0]["content"]).contains("USER-QUESTION-8821"));
    assert!(!texts(&messages[0]["content"]).contains(TRUSTED));
}
