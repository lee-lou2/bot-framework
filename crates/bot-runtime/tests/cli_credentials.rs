//! Verifies with the real `claude` CLI (builtin Read enabled), PostgreSQL, and broker front that the session token is never stored in any model-readable file.
//! The Bearer token is injected only via `RELAY_TOKEN` (`${RELAY_TOKEN}` placeholder in MCP config) and never appears in requests or `/bot`/`/work` files.
//! Run: `BOT_CLAUDE_BIN=<path> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-runtime --test cli_credentials`

mod support;

use bot_broker::testing::FakeUpstream;
use bot_core::{id::*, profile::Builtin};
use bot_runtime::Outcome;
use std::{sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

/// Relative paths resolve against CLI cwd (`/bot`); `..` probes `/work` outside the input directory.
const READS: [(&str, &str, &str); 6] = [
    ("toolu_in", "Read", r#"{"file_path":"AGENTS.md"}"#),
    (
        "toolu_cfg",
        "Read",
        r#"{"file_path":"../work/mcp.claude.json"}"#,
    ),
    (
        "toolu_cli",
        "Read",
        r#"{"file_path":"../work/claude/.claude.json"}"#,
    ),
    (
        "toolu_home",
        "Read",
        r#"{"file_path":"../work/home/.claude.json"}"#,
    ),
    ("toolu_host", "Read", r#"{"file_path":"/etc/hosts"}"#),
    ("toolu_mcp", "mcp__relay__agenda_list", "{}"),
];

#[tokio::test]
async fn the_real_front_accepts_the_token_from_the_environment_and_no_readable_file_or_model_request_holds_it()
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
        .builtin = [Builtin::Read].into();
    w.catalog.publish(Arc::new(snapshot));

    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(FakeUpstream::new());
    script(&upstream, tools_turn("msg_c1", &READS));
    script(&upstream, final_turn("msg_c2", "Read complete"));

    let run = w.accept("req-credentials", "Read the files").await;
    let host = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace("cred"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.pg.state_of(&run).await, "succeeded");

    let seen = bodies(&upstream);
    assert_eq!(
        seen.len(),
        2,
        "expected one tool-call turn and one final turn"
    );
    let result = |id: &str| result_text(&seen[1], id).unwrap_or_else(|| panic!("missing {id}"));
    let mcp = result("toolu_mcp");
    assert!(
        mcp.contains("Walk"),
        "relay tool must return real data: {mcp}"
    );
    assert_eq!(result_error(&seen[1], "toolu_mcp"), Some(false));
    assert_eq!(w.runner.jobs().len(), 1);

    // Control: files inside `/bot` are readable, proving denials below come from confinement.
    let agents = result("toolu_in");
    assert_eq!(result_error(&seen[1], "toolu_in"), Some(false), "{agents}");
    assert!(
        agents.starts_with("1\t"),
        "expected numbered lines: {agents}"
    );
    for id in ["toolu_cfg", "toolu_cli", "toolu_home", "toolu_host"] {
        assert_eq!(result_error(&seen[1], id), Some(true), "{id}");
        assert!(
            result(id).contains("requested permissions to read"),
            "{id}: {}",
            result(id)
        );
    }

    let tokens = host.tokens();
    assert!(!tokens.is_empty());
    for body in &seen {
        let text = body.to_string();
        for token in &tokens {
            assert!(
                !text.contains(token.as_str()),
                "token leaked into model request"
            );
        }
    }

    let swept = sandbox.swept();
    assert_eq!(swept.len(), 1, "{swept:?}");
    assert!(swept[0].token_len >= 16, "{swept:?}");
    assert!(swept[0].files >= 3, "{swept:?}");
    assert!(
        swept[0].hits.is_empty(),
        "session token found in mounted directory: {:?}",
        swept[0].hits
    );
}
