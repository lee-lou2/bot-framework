//! Verifies host `serve` wiring for CLI preflight refusal markers (`PlannedCalls::with_preflight`):
//! calls rejected by tool input validation persist a refusal marker in the run checkpoint.
//!
//! Run: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-cli --test claude_wiring`
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use claude_support::*;
use process_support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cli_call_the_broker_confirms_as_refused_leaves_the_refusal_marker_in_the_run_checkpoint()
{
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    if !have_srt() {
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("w")
        .tempdir_in("/tmp")
        .unwrap();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let profile = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run, calendar.read]
members:
  - {identity: "local:owner", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
secrets: []
policy:
  scopes: [run, calendar.read]
  approve: []
"#;
    let tools = "version: 1\nserver: agenda\ntools:\n  - id: list\n    class: read\n    scopes: [calendar.read]\n    secrets: {}\n    egress: []\n    input: {type: object, properties: {}, additionalProperties: false}\n";
    let rig = Rig::start_hosted(
        schema,
        claude,
        profile,
        &[
            ("agents/main/skills/agenda/SKILL.md", "Queries the agenda."),
            ("agents/main/skills/agenda/tools.yaml", tools),
        ],
        &cli_gateway(),
        &format!(
            r#", "servers": {{"agenda": {{"root": "{}", "command": ["bin/server"]}}}}"#,
            pkg.display()
        ),
        &[Role::Gateway, Role::Worker],
    )
    .await;
    // Undeclared arg `bogus` triggers a pre-execution `Invalid` refusal; preflight records a marker.
    rig.script(tool_turn(
        "msg_w1",
        "toolu_w1",
        "mcp__relay__agenda_list",
        r#"{"bogus":1}"#,
    ));
    rig.script(final_turn(
        "msg_w2",
        "Could not query the agenda due to invalid tool arguments",
    ));
    let out = rig.chat("w1", "Show today's agenda").await;
    assert!(out.status.success(), "{}", said(&out));
    assert!(
        said(&out).contains("invalid tool arguments"),
        "{}",
        said(&out)
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_steps where key like 'cli-refusal-%'"),
        "1",
        "confirmed refusal marker must be persisted in run checkpoint"
    );
    assert_eq!(
        rig.query("select body from {s}.bot_steps where key like 'cli-refusal-%'"),
        "\"invalid\""
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_calls where state = 'started'"),
        "0"
    );
}
