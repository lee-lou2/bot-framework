//! End-to-end host wiring acceptance test for `PrivateIntake`: fake Slack DM -> `PrivateGate` -> designated tool in `srt` sandbox with zero plaintext persisted in PG.
mod claude_support;
mod process_support;

use bot_broker::testing::FakeSecrets;
use bot_cli::cli::Role;
use bot_gateway::testing::slack::{FakeSlack, dm_frame};
use claude_support::*;
use process_support::{Schema, have_srt_and_python, package};
use std::{
    fs::{File, TryLockError},
    path::PathBuf,
    sync::Arc,
};

const SECRET_TEXT: &str = "TOP-SECRET-DM-TEXT-7731";

const SERVER: &str = r#"#!/usr/bin/env python3
import json, sys

EXPECTED = "TOP-SECRET-DM-TEXT-7731"
TOOLS = [{"name": "receive", "description": "Forwards received text",
          "inputSchema": {"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}, "additionalProperties": False}}]

def reply(message_id, **body):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message_id, **body}) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    if "id" not in message:
        continue
    method, identity = message.get("method"), message["id"]
    if method == "initialize":
        reply(identity, result={"protocolVersion": message["params"].get("protocolVersion", "2025-06-18"),
                                "capabilities": {"tools": {"listChanged": False}}, "serverInfo": {"name": "inbox", "version": "0"}})
    elif method == "ping":
        reply(identity, result={})
    elif method == "tools/list":
        reply(identity, result={"tools": TOOLS})
    elif method == "tools/call":
        text = (message["params"].get("arguments") or {}).get("text")
        ok = text == EXPECTED
        reply(identity, result={"content": [{"type": "text", "text": "ok" if ok else "wrong"}], "isError": not ok})
    else:
        reply(identity, error={"code": -32601, "message": "method not found"})
"#;

const PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run, inbox.write]
members:
  - {identity: "slack:home/U1", roles: [owner]}
bindings:
  - {id: dm, adapter: slack, workspace: home, dm: true, agent: main}
secrets: []
policy:
  scopes: [run, inbox.write]
  approve: []
"#;

const TOOLS: &str = "version: 1\nserver: inbox\ntools:\n  - id: receive\n    class: write\n    scopes: [inbox.write]\n    secrets: {}\n    egress: []\n    input: {type: object, required: [text], properties: {text: {type: string}}, additionalProperties: false}\n";
const TRIGGERS: &str = "version: 1\ntriggers:\n  - id: secure\n    \"on\": {event: message, binding: dm}\n    private: true\n    target: {tool: inbox.receive}\n    input: {text: body.text}\n";

fn secrets() -> Arc<FakeSecrets> {
    Arc::new(FakeSecrets::new(&[
        ("personal", "model-key", "provider-key-for-personal"),
        ("personal", "bot-token", "xoxb-test-token"),
        ("personal", "app-token", "xapp-test-token"),
        // Per-profile fingerprint key required to enable private intake.
        ("personal", "private-intake", "profile-intake-key-bytes"),
    ]))
}

fn rows_containing(rig: &Rig, needle: &str) -> usize {
    let tables = rig.query(
        "select string_agg(table_name::text, ',') from information_schema.tables \
         where table_schema = '{s}' and table_type = 'BASE TABLE'",
    );
    tables
        .split(',')
        .map(|table| {
            rig.query(&format!(
                "select count(*) from {{s}}.\"{table}\" t where t::text like '%{needle}%'"
            ))
            .parse::<usize>()
            .unwrap()
        })
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_dm_reaches_only_the_designated_tool_in_the_srt_sandbox_and_leaves_no_plaintext()
{
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    if !have_srt_and_python() {
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("p")
        .tempdir_in("/tmp")
        .unwrap();
    let package = package(dir.path(), SERVER);
    let slack = FakeSlack::start().await;
    let rig = Rig::launch(
        schema,
        PathBuf::from("/nonexistent/claude"),
        PROFILE,
        &[
            (
                "agents/main/skills/inbox/SKILL.md",
                "Forwards received text.",
            ),
            ("agents/main/skills/inbox/tools.yaml", TOOLS),
            ("agents/main/triggers.yaml", TRIGGERS),
        ],
        &route_gateway(),
        &[Role::Gateway],
        secrets(),
        &format!(
            r#", "servers": {{"inbox": {{"root": "{}", "command": ["bin/server", "--stdio"]}}}}"#,
            package.display()
        ),
        Some(&slack.base),
    )
    .await;
    let mut rig = rig;
    rig.until("gateway did not publish an active revision", 60, |rig| {
        !rig.schema.active().is_empty()
    })
    .await;
    assert!(slack.connected().await, "Slack socket did not connect");

    let first = ts(1);
    slack
        .push(dm_frame("E1", "T1", "D1", "U1", &first, SECRET_TEXT))
        .await;
    wait(&rig, "private intake did not succeed", |rig| {
        rig.query("select count(*) from {s}.bot_runs where private and state = 'succeeded'") == "1"
    })
    .await;
    assert!(
        slack
            .until(|slack| slack.acks().iter().any(|id| id == "E1"))
            .await,
        "event must be acked after intake finishes"
    );
    // Keep the install lock fd open across `stop` because tempdir cleanup removes the path.
    let lock = std::fs::read_dir(rig.dir.path().join("installs"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "lock"))
        .expect("install usage lock file");
    let probe = File::open(&lock).unwrap();
    assert!(
        matches!(probe.try_lock(), Err(TryLockError::WouldBlock)),
        "exclusive lock must be blocked while private intake holds the install"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where not private"),
        "0"
    );
    assert_eq!(
        rows_containing(&rig, SECRET_TEXT),
        0,
        "plaintext leaked into store"
    );
    // Redelivered Slack event must be deduplicated without invoking the tool again.
    slack
        .push(dm_frame("E1b", "T1", "D1", "U1", &first, SECRET_TEXT))
        .await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where private"),
        "1",
        "redelivery must not create a second private run"
    );

    // Failed write-tool reply leaves an open operator hold instead of succeeding or falling back to normal chat.
    slack
        .push(dm_frame("E2", "T1", "D1", "U1", &ts(2), "other-text"))
        .await;
    wait(&rig, "second intake was not held", |rig| {
        rig.query("select count(*) from {s}.bot_holds where kind = 'private' and closed_ms is null")
            == "1"
    })
    .await;
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where private and state = 'succeeded'"),
        "1",
        "tool failure must not mark private intake succeeded"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where not private"),
        "0",
        "must not fall back to regular chat"
    );
    assert_eq!(rows_containing(&rig, "other-text"), 0);
    rig.stop().await;
    assert!(
        probe.try_lock().is_ok(),
        "exclusive lock must be released after shutdown"
    );
}
