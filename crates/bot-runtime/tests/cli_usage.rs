//! Verifies with the real `claude` CLI (fake upstream) and PostgreSQL that run usage is stored only when every model request in the run reported usage.
//! Run: `BOT_CLAUDE_BIN=<path> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-runtime --test cli_usage`

mod support;

use bot_broker::testing::FakeUpstream;
use bot_runtime::Outcome;
use std::{sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

async fn stored(first: Vec<Vec<u8>>, last: Vec<Vec<u8>>, tag: &str) -> Option<(String, String)> {
    let w = world().await?;
    let claude = claude_bin()?;
    let upstream = Arc::new(FakeUpstream::new());
    script(&upstream, first);
    script(&upstream, last);
    let run = w.accept(&format!("req-{tag}"), "List my agenda").await;
    let host = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        LocalSandbox::new(claude),
        &workspace(tag),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded), "{tag}");
    assert_eq!(upstream.requests().len(), 2, "{tag}");
    let usage = w
        .pg
        .scalar::<String>(&format!(
            "select coalesce(usage_prompt::text, '-') || '/' || coalesce(usage_completion::text, '-') || '/' || coalesce(truncated::text, '-') from bot_runs where id = '{}'",
            run.0
        ))
        .await;
    let text =
        w.pg.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            run.0
        ))
        .await;
    Some((usage, text))
}

fn tool() -> Vec<Vec<u8>> {
    tool_turn("msg_first", "toolu_first", "mcp__relay__agenda_list", "{}")
}

#[tokio::test]
async fn usage_is_stored_only_when_every_model_request_of_the_run_was_reported() {
    let Some((usage, _)) = stored(tool(), final_turn("msg_last", "Final answer"), "u-all").await
    else {
        return;
    };
    assert_eq!(usage, "14/8/false");
    let (usage, _) = stored(
        unreported(tool()),
        final_turn("msg_last", "Final answer"),
        "u-first",
    )
    .await
    .unwrap();
    assert_eq!(usage, "-/-/false");
    let (usage, _) = stored(
        tool(),
        unreported(final_turn("msg_last", "Final answer")),
        "u-last",
    )
    .await
    .unwrap();
    assert_eq!(usage, "-/-/false");
    let (usage, _) = stored(
        unreported(tool()),
        unreported(final_turn("msg_last", "Final answer")),
        "u-none",
    )
    .await
    .unwrap();
    assert_eq!(usage, "-/-/false");
}

#[tokio::test]
async fn a_cut_answer_the_cli_continues_is_measured_over_all_its_requests_and_is_not_truncated() {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let upstream = Arc::new(FakeUpstream::new());
    script(&upstream, cut_turn("msg_cut", "Truncated prefix"));
    script(&upstream, final_turn("msg_end", "Continued ending"));
    let run = w.accept("req-cut-continued", "Write a long answer").await;
    let host = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        LocalSandbox::new(claude),
        &workspace("u-cut"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(upstream.requests().len(), 2);
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select usage_prompt::text || '/' || usage_completion::text || '/' || truncated::text from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        "18/8/false"
    );
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        r#"{"text":"Continued ending"}"#
    );
}
