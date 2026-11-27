//! Built-in `delegate`: direct children only, bounded by both revisions, idempotent per step, parking retires only that attempt's session.

use super::*;
use crate::front::{cli::PlannedCalls, park::ParkReason};

const SCOPES: [&str; 3] = ["run", "echo.use", "echo.drop"];

pub(super) fn family(revision: &str) -> crate::testing::SnapshotBuilder {
    snapshot("personal", revision)
        .role("owner", &SCOPES)
        .member("local:owner", &["owner"])
        .policy_scopes(&SCOPES)
        .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
        .tool(
            "main",
            "echo.drop",
            tool(Risk::Irreversible, &["echo.drop"]),
        )
        .agent("main/research")
        .agent("main/research/deep")
        .agent("main/ops")
}

pub(super) async fn with_family() -> Rig {
    let r = rig_with(
        |_| {},
        |_| {},
        Some(Box::new(|ledger, _| {
            Arc::new(
                PlannedCalls::new(ledger.clone(), Duration::from_millis(200))
                    .with_poll(Duration::from_millis(10)),
            )
        })),
    )
    .await;
    r.catalog.publish(family("r1").build());
    r
}

pub(super) fn delegate(id: &str, args: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"delegate","arguments":{args},"_meta":{{"claudecode/toolUseId":"{id}"}}}}}}"#
    )
}

/// Records the planned tool-use checkpoints normally written by the turn stream.
pub(super) fn plan(r: &Rig, turn: u64, id: &str, input: &str) {
    r.ledger.add_step(
        "r1",
        &format!("cli-use-{id}"),
        &format!(
            r#"{{"seq":{turn},"turn":{turn},"pos":0,"name":"mcp__relay__delegate","input":{input}}}"#
        ),
    );
    r.ledger.add_step(
        "r1",
        &format!("cli-group-{turn}"),
        &format!(r#"{{"seq":{turn},"uses":["{id}"]}}"#),
    );
}

#[tokio::test]
async fn the_tool_list_names_exactly_the_direct_children() {
    let r = with_family().await;
    let list = rpc(&r, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
    let value: serde_json::Value = serde_json::from_str(&list.body).unwrap();
    let tool = value["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "delegate")
        .expect("delegate");
    assert_eq!(
        tool["inputSchema"]["properties"]["agent"]["enum"],
        serde_json::json!(["main/ops", "main/research"]),
        "grandchildren must not appear"
    );
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    let r = plain().await;
    assert!(
        !rpc(&r, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .body
            .contains("delegate")
    );
}

#[tokio::test]
async fn a_delegation_starts_the_child_once_parks_the_attempt_and_retires_only_its_session() {
    let r = with_family().await;
    let input = r#"{"agent":"main/research","instruction":"summarize the week"}"#;
    plan(&r, 4, "toolu_d", input);
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(reply.body.contains("child_pending"), "{}", reply.body);
    let commands = r.delegation.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(
        (
            commands[0].step.as_str(),
            commands[0].child.0.as_str(),
            commands[0].instruction.as_str()
        ),
        ("cli-4-0", "main/research", "summarize the week"),
        "step key must equal the logical call ID"
    );
    assert_eq!(commands[0].parent, r.attempt);
    assert!(
        commands[0]
            .scopes
            .iter()
            .all(|s| SCOPES.contains(&s.as_str()))
    );
    assert_eq!(r.park.seen(), [(r.attempt.clone(), ParkReason::Child)]);
    // Parked attempt's session is retired so the CLI cannot call the model again.
    assert_eq!(rpc(&r, &delegate("toolu_x", input)).await.status, 401);
    assert_eq!(r.delegation.commands().len(), 1);
}

#[tokio::test]
async fn only_a_direct_child_in_both_revisions_may_be_delegated_to() {
    let r = with_family().await;
    let mut n = 0;
    for (agent, word) in [
        ("main/research/deep", "denied"), // grandchild
        ("main/ops/none", "denied"),      // missing agent
        ("main", "denied"),               // self
        ("other", "denied"),
    ] {
        n += 1;
        let input = format!(r#"{{"agent":"{agent}","instruction":"x"}}"#);
        plan(&r, n, &format!("toolu_{n}"), &input);
        let reply = rpc(&r, &delegate(&format!("toolu_{n}"), &input)).await;
        assert!(reply.body.contains(word), "{agent}: {}", reply.body);
    }
    for (i, input) in [
        r#"{"agent":"main/research"}"#,
        r#"{"agent":"main/research","instruction":""}"#,
        r#"{"agent":"main/research","instruction":"x","extra":1}"#,
        r#"{"agent":5,"instruction":"x"}"#,
    ]
    .iter()
    .enumerate()
    {
        let id = format!("toolu_f{i}");
        plan(&r, 20 + i as u64, &id, input);
        let reply = rpc(&r, &delegate(&id, input)).await;
        assert!(reply.body.contains("invalid"), "{input}: {}", reply.body);
    }
    assert!(r.delegation.commands().is_empty());
    assert!(r.park.seen().is_empty());
    // Refused when the child did not exist in the pinned revision.
    let pinned = snapshot("personal", "r1")
        .role("owner", &SCOPES)
        .member("local:owner", &["owner"])
        .policy_scopes(&SCOPES)
        .agent("main/ops")
        .build();
    r.catalog.publish(pinned);
    r.catalog.publish(family("r2").build());
    r.ledger.set_active_revision("personal", "r2");
    let input = r#"{"agent":"main/research","instruction":"x"}"#;
    plan(&r, 40, "toolu_p", input);
    assert!(
        rpc(&r, &delegate("toolu_p", input))
            .await
            .body
            .contains("denied")
    );
    assert!(r.delegation.commands().is_empty());
}

#[tokio::test]
async fn the_child_gets_only_the_scopes_the_requester_holds_in_both_revisions() {
    let r = with_family().await;
    r.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &["run", "echo.use"],
    ));
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    plan(&r, 5, "toolu_s", input);
    rpc(&r, &delegate("toolu_s", input)).await;
    let scopes: Vec<String> = r.delegation.commands()[0].scopes.iter().cloned().collect();
    assert_eq!(scopes, ["echo.use", "run"]);
    // Scopes removed from the requester's role in the current revision are dropped.
    let r = with_family().await;
    r.catalog
        .publish(family("r1").role("owner", &["run"]).build());
    plan(&r, 5, "toolu_s", input);
    rpc(&r, &delegate("toolu_s", input)).await;
    assert_eq!(
        r.delegation.commands()[0]
            .scopes
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["run"]
    );
}

/// Records a child started with `scopes` and completed with its result checkpointed on the parent.
async fn finished_child(r: &Rig, instruction: &str, scopes: &[&str], body: &str) {
    let command = bot_core::command::Delegate {
        parent: r.attempt.clone(),
        checked_revision: Revision("r1".into()),
        step: "cli-4-0".into(),
        child: AgentId("main/research".into()),
        instruction: instruction.into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
    };
    use bot_core::port::DelegationStore;
    r.delegation.delegate(command).await.unwrap();
    r.delegation.finish_child("personal", "r1", "cli-4-0", body);
    r.ledger.add_step("r1", "cli-4-0", body);
}

#[tokio::test]
async fn a_finished_child_returns_its_result_to_the_same_tool_use() {
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    let r = with_family().await;
    plan(&r, 4, "toolu_d", input);
    finished_child(
        &r,
        "go",
        &SCOPES,
        r#"{"state":"succeeded","result":"the week was calm"}"#,
    )
    .await;
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("the week was calm") && reply.body.contains(r#""isError":false"#),
        "{}",
        reply.body
    );
    assert_eq!(r.delegation.commands().len(), 1, "must not restart child");
    assert!(r.park.seen().is_empty());
    // Failed or cancelled children expose fixed error codes only.
    let r = with_family().await;
    plan(&r, 4, "toolu_d", input);
    finished_child(
        &r,
        "go",
        &SCOPES,
        r#"{"state":"dead","error":"secret-internal-detail"}"#,
    )
    .await;
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("child_failed") && !reply.body.contains("secret-internal-detail"),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_finished_childs_result_is_hidden_when_the_revision_moved_in_the_store() {
    let r = with_family().await;
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    plan(&r, 4, "toolu_d", input);
    finished_child(&r, "go", &SCOPES, r#"{"state":"succeeded","result":"x"}"#).await;
    r.delegation.set_active("r2");
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("stale") && !reply.body.contains("\"x\""),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_stored_child_result_needs_the_original_scopes_inside_the_current_ones_and_the_same_target()
 {
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    // Original child scopes exceed currently authorized scopes: withhold result.
    let r = with_family().await;
    plan(&r, 4, "toolu_d", input);
    finished_child(
        &r,
        "go",
        &["run", "echo.use", "echo.drop", "calendar.admin"],
        r#"{"state":"succeeded","result":"secret result"}"#,
    )
    .await;
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("denied") && !reply.body.contains("secret result"),
        "{}",
        reply.body
    );
    // Different instruction on the same step conflicts.
    let r = with_family().await;
    plan(&r, 4, "toolu_d", input);
    finished_child(
        &r,
        "something else",
        &SCOPES,
        r#"{"state":"succeeded","result":"secret result"}"#,
    )
    .await;
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("conflict") && !reply.body.contains("secret result"),
        "{}",
        reply.body
    );
    // Checkpoint present while delegation store reports unfinished: do not return it.
    let r = with_family().await;
    plan(&r, 4, "toolu_d", input);
    r.ledger
        .add_step("r1", "cli-4-0", r#"{"state":"succeeded","result":"early"}"#);
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(!reply.body.contains("early"), "{}", reply.body);
}

#[tokio::test]
async fn delegation_needs_the_planned_call_and_a_current_session() {
    let r = with_family().await;
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    assert!(
        rpc(&r, &delegate("toolu_d", input))
            .await
            .body
            .contains("unavailable")
    );
    plan(
        &r,
        4,
        "toolu_e",
        r#"{"agent":"main/ops","instruction":"go"}"#,
    );
    assert!(
        rpc(&r, &delegate("toolu_e", input))
            .await
            .body
            .contains("conflict")
    );
    assert!(r.delegation.commands().is_empty());
    plan(&r, 5, "toolu_f", input);
    r.ledger.bump_claim("r1");
    assert!(
        rpc(&r, &delegate("toolu_f", input))
            .await
            .body
            .contains("stale")
    );
    assert!(r.delegation.commands().is_empty());
}

#[tokio::test]
async fn an_approval_or_a_hold_parks_the_attempt_and_retires_its_session_but_not_another_claims() {
    let r = with_family().await;
    let drop = r#"{"agent":"x"}"#;
    let _ = drop;
    let call = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"echo_drop","arguments":{},"_meta":{"claudecode/toolUseId":"toolu_g"}}}"#;
    r.ledger.add_step(
        "r1",
        "cli-use-toolu_g",
        r#"{"seq":2,"turn":2,"pos":0,"name":"mcp__relay__echo_drop","input":{}}"#,
    );
    r.ledger
        .add_step("r1", "cli-group-2", r#"{"seq":2,"uses":["toolu_g"]}"#);
    // A session issued for a newer claim on the same run must remain active.
    let mut newer = r.attempt.clone();
    newer.claim = Some(bot_core::id::ClaimEpoch(2));
    let other = r
        .sessions
        .issue(Session {
            attempt: newer.clone(),
            agent: AgentId("main".into()),
            alias: "assistant".into(),
        })
        .unwrap();
    let reply = rpc(&r, call).await;
    assert!(reply.body.contains("approval_pending"), "{}", reply.body);
    assert_eq!(r.park.seen(), [(r.attempt.clone(), ParkReason::Approval)]);
    assert_eq!(
        rpc(&r, call).await.status,
        401,
        "parked attempt session must be retired"
    );
    let token = format!("Bearer {other}");
    let still = raw(
        r.front.addr(),
        &post(
            "/mcp",
            Some(&token),
            "authorization",
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        ),
    )
    .await;
    assert_eq!(still.status, 200, "newer claim session must remain active");
}

fn refused(r: &Rig, body: &str, word: &str) {
    assert!(body.contains(word), "{body}");
    assert!(r.delegation.commands().is_empty());
    assert!(r.park.seen().is_empty());
}

#[tokio::test]
async fn a_requester_without_run_in_either_revision_cannot_start_or_resume_a_delegation() {
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    // Current revision lacks `run`: refuse both new delegation and stored child result.
    let r = with_family().await;
    r.catalog.publish(
        family("r1")
            .role("owner", &["echo.use", "echo.drop"])
            .build(),
    );
    plan(&r, 4, "toolu_d", input);
    refused(
        &r,
        &rpc(&r, &delegate("toolu_d", input)).await.body,
        "denied",
    );
    r.ledger.add_step(
        "r1",
        "cli-4-0",
        r#"{"state":"succeeded","result":"secret result"}"#,
    );
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("denied") && !reply.body.contains("secret result"),
        "{}",
        reply.body
    );
    // Accepted run scopes lack `run`.
    let r = with_family().await;
    r.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &["echo.use", "echo.drop"],
    ));
    plan(&r, 4, "toolu_d", input);
    refused(
        &r,
        &rpc(&r, &delegate("toolu_d", input)).await.body,
        "denied",
    );
    // Pinned revision lacked `run` even though current revision grants it.
    let r = with_family().await;
    r.catalog.publish(
        family("r1")
            .role("owner", &["echo.use", "echo.drop"])
            .build(),
    );
    r.catalog.publish(family("r2").build());
    r.ledger.set_active_revision("personal", "r2");
    plan(&r, 4, "toolu_d", input);
    refused(
        &r,
        &rpc(&r, &delegate("toolu_d", input)).await.body,
        "denied",
    );
}

#[tokio::test]
async fn a_requester_who_is_no_longer_a_member_cannot_delegate_or_read_a_stored_child_result() {
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    let r = with_family().await;
    let without_member = snapshot("personal", "r1")
        .role("owner", &SCOPES)
        .member("local:someone-else", &["owner"])
        .policy_scopes(&SCOPES)
        .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
        .agent("main/research")
        .build();
    r.catalog.publish(without_member);
    plan(&r, 4, "toolu_d", input);
    refused(
        &r,
        &rpc(&r, &delegate("toolu_d", input)).await.body,
        "denied",
    );
    r.ledger.add_step(
        "r1",
        "cli-4-0",
        r#"{"state":"succeeded","result":"secret result"}"#,
    );
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("denied") && !reply.body.contains("secret result"),
        "{}",
        reply.body
    );
}

#[test]
fn delegation_scopes_is_one_function_for_every_entry_point() {
    use bot_core::scope::{Denial, delegation_scopes};
    let full = family("r1").build();
    let ctx = run_context("personal", "r1", 1, "local:owner", "main", &SCOPES);
    let child = AgentId("main/research".into());
    let scopes = delegation_scopes(&ctx, &full, &full, &child).unwrap();
    assert!(scopes.contains("run") && scopes.contains("echo.use"));
    for other in ["main/research/deep", "main/ghost", "main"] {
        assert_eq!(
            delegation_scopes(&ctx, &full, &full, &AgentId(other.into())),
            Err(Denial::Agent),
            "{other}"
        );
    }
    let outsider = run_context("personal", "r1", 1, "local:nobody", "main", &SCOPES);
    assert_eq!(
        delegation_scopes(&outsider, &full, &full, &child),
        Err(Denial::Member)
    );
    let no_run = family("r1").role("owner", &["echo.use"]).build();
    assert_eq!(
        delegation_scopes(&ctx, &no_run, &full, &child),
        Err(Denial::Missing(vec!["run".into()]))
    );
    assert_eq!(
        delegation_scopes(&ctx, &full, &no_run, &child),
        Err(Denial::Missing(vec!["run".into()]))
    );
}
