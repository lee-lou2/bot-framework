//! Unit tests for `RunSession` (`SessionPort`) covering commits, response validation, recall, and slot settlement.

use super::fakes::*;
use crate::{
    host::Relays,
    model::{
        ModelRequest, ModelRunner, ModelStep, RelayChannel, RelayGrant, RunnerKind, Session,
        SessionError, Slot, StepFuture, ToolOutput,
        session::{Pending, ToolUse},
    },
    worker::Outcome,
};
use bot_core::{
    cli::{CliResponse, CliResponseUse},
    command::*,
    id::*,
    ledger::{CallPhase, CallRecord},
    port::Error,
    state::RunContext,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

struct Grab(Arc<Mutex<Option<Session>>>);

impl ModelRunner for Grab {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        *lock(&self.0) = request.session;
        Box::pin(async {
            Ok(ModelStep::Final {
                text: "reply".into(),
                usage: Default::default(),
                outcome: Default::default(),
            })
        })
    }
}

struct Open;

impl Relays for Open {
    fn open(&self, _: &Attempt, _: &AgentId, _: &str) -> Option<RelayGrant> {
        Some(RelayGrant::new(
            RelayChannel {
                name: "relay".into(),
                base_url: "http://192.168.64.1:8787".into(),
                token: "tok".into(),
            },
            (),
        ))
    }
}

async fn session_of(ctx: RunContext, relays: bool) -> (Rig, Option<Session>) {
    let grabbed = Arc::new(Mutex::new(None));
    let model: Arc<dyn ModelRunner> = Arc::new(Grab(grabbed.clone()));
    let rig = if relays {
        rig_with_relays(model, Arc::new(Open))
    } else {
        rig(model)
    };
    assert_eq!(rig.worker.execute(ctx).await, Outcome::Succeeded);
    let session = lock(&grabbed).take();
    (rig, session)
}

fn pending(turn: u64, pos: u32, name: &str, input: serde_json::Value) -> Pending {
    Pending {
        turn,
        pos,
        tool_use: ToolUse {
            id: format!("toolu_{turn}_{pos}"),
            name: name.into(),
            input,
        },
    }
}

fn output(slot: Slot) -> ToolOutput {
    match slot {
        Slot::Result(output) => output,
        Slot::Parked => panic!("expected Slot::Result"),
    }
}

#[tokio::test]
async fn a_loop_run_gets_a_session_port_only_when_a_relay_is_open() {
    let (_, session) = session_of(agent_run("r1"), true).await;
    assert!(session.is_some());
    let (_, session) = session_of(agent_run("r2"), false).await;
    assert!(
        session.is_none(),
        "no session port without an open relay channel"
    );
}

#[tokio::test]
async fn the_port_sees_the_claim_steps_and_commits_a_batch_through_the_run_store() {
    let mut ctx = agent_run("r1");
    ctx.work.steps.push(Step {
        key: "cli-rec-0".into(),
        body: JsonPayload("\"{}\"".into()),
    });
    let (rig, session) = session_of(ctx, true).await;
    let port = session.unwrap().0;
    assert_eq!(port.steps().len(), 1);
    let batch = vec![
        Step {
            key: "cli-rec-1".into(),
            body: JsonPayload("\"a\"".into()),
        },
        Step {
            key: "cli-group-1".into(),
            body: JsonPayload("{}".into()),
        },
    ];
    port.commit(batch).await.unwrap();
    assert_eq!(
        rig.runs.events(),
        ["checkpoint:cli-rec-1", "checkpoint:cli-group-1"]
    );
    let clash = vec![Step {
        key: "cli-rec-1".into(),
        body: JsonPayload("\"b\"".into()),
    }];
    assert_eq!(
        port.commit(clash).await,
        Err(SessionError::Permanent("checkpoint_conflict"))
    );
}

#[tokio::test]
async fn the_front_confirmed_response_is_read_and_validated() {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    assert_eq!(port.response("msg_1").await, Ok(None));
    let good = CliResponse {
        id: "msg_1".into(),
        uses: vec![CliResponseUse {
            id: "toolu_1".into(),
            name: "mcp__relay__agenda_list".into(),
            input: json!({}),
        }],
    };
    lock(&rig.ledger.steps).insert(
        "cli-resp-msg_1".into(),
        JsonPayload(serde_json::to_string(&good).unwrap()),
    );
    assert_eq!(port.response("msg_1").await, Ok(Some(good)));
    lock(&rig.ledger.steps).insert(
        "cli-resp-msg_2".into(),
        JsonPayload("{\"id\":\"msg_2\",\"uses\":[]}".into()),
    );
    assert_eq!(
        port.response("msg_2").await,
        Err(SessionError::Permanent("cli_response_invalid"))
    );
    rig.ledger
        .down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(port.response("msg_1").await, Err(SessionError::Unavailable));
}

#[tokio::test]
async fn recalling_an_answered_tool_reads_only_finished_calls_through_the_broker_and_never_executes()
 {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let slot = pending(7, 0, "mcp__relay__agenda_list", json!({"b": 1, "a": 2}));
    let record = |phase| CallRecord {
        call: CallId::cli(7, 0),
        tool: ToolId("agenda.list".into()),
        args_sha256: [0; 32],
        phase,
    };
    assert_eq!(port.recall(&slot).await, Ok(None));
    lock(&rig.ledger.calls).insert("cli-7-0".into(), record(CallPhase::Open { approval: None }));
    assert_eq!(port.recall(&slot).await, Ok(None));
    assert!(rig.broker.calls().is_empty());
    lock(&rig.ledger.calls).insert(
        "cli-7-0".into(),
        record(CallPhase::Done(JsonPayload("{}".into()))),
    );
    rig.broker.script(done(
        r#"{"content":[{"type":"text","text":"actual"}],"isError":true}"#,
    ));
    let out = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(
        (out.content[0]["text"].clone(), out.is_error),
        (json!("actual"), true)
    );
    let sent = rig.broker.calls();
    assert_eq!((sent.len(), sent[0].call.0.as_str()), (1, "cli-7-0"));
    assert_eq!(sent[0].args.0, r#"{"a":2,"b":1}"#);
    let canonical_hash = bot_core::canon::sha256(r#"{"a":2,"b":1}"#);
    let denied_record = |hash: [u8; 32], tool_id: &str| CallRecord {
        call: CallId::cli(7, 0),
        tool: ToolId(tool_id.into()),
        args_sha256: hash,
        phase: CallPhase::Denied,
    };
    let calls_before = rig.broker.calls().len();
    lock(&rig.ledger.calls).insert(
        "cli-7-0".into(),
        denied_record(canonical_hash, "agenda.list"),
    );
    for _ in 0..2 {
        let again = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(
            (again.content[0]["text"].clone(), again.is_error),
            (json!("denied"), true)
        );
    }
    assert_eq!(
        rig.broker.calls().len(),
        calls_before,
        "confirmed denial must not call the broker again"
    );
    // Approval digests may include sealed extra context, so a differing digest on the same tool still recalls as denied.
    lock(&rig.ledger.calls).insert("cli-7-0".into(), denied_record([9; 32], "agenda.list"));
    for _ in 0..2 {
        let sealed = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(sealed.content[0]["text"].clone(), json!("denied"));
    }
    lock(&rig.ledger.calls).insert(
        "cli-7-0".into(),
        denied_record(canonical_hash, "agenda.delete"),
    );
    assert_eq!(port.recall(&slot).await, Ok(None));
    lock(&rig.ledger.calls).insert(
        "cli-7-0".into(),
        record(CallPhase::Done(JsonPayload("{}".into()))),
    );
    rig.broker.script(Err(Error::Denied));
    let denied = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(
        (denied.content[0]["text"].clone(), denied.is_error),
        (json!("denied"), true)
    );
    let read = pending(7, 1, "Read", json!({"file_path": "/bot/AGENTS.md"}));
    assert_eq!(port.recall(&read).await, Ok(None));
}

#[tokio::test]
async fn a_call_has_ended_only_when_the_ledger_shows_it_done_or_confirmed_denied() {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let slot = pending(7, 0, "mcp__relay__agenda_list", json!({"a": 1}));
    let record = |phase| CallRecord {
        call: CallId::cli(7, 0),
        tool: ToolId("agenda.list".into()),
        args_sha256: [0; 32],
        phase,
    };
    assert_eq!(port.ended(&slot).await, Ok(false));
    for phase in [
        CallPhase::Open { approval: None },
        CallPhase::Held {
            reason: "timeout".into(),
            resolution: None,
        },
    ] {
        lock(&rig.ledger.calls).insert("cli-7-0".into(), record(phase));
        assert_eq!(port.ended(&slot).await, Ok(false));
    }
    for phase in [CallPhase::Done(JsonPayload("{}".into())), CallPhase::Denied] {
        lock(&rig.ledger.calls).insert("cli-7-0".into(), record(phase));
        assert_eq!(port.ended(&slot).await, Ok(true));
    }
    assert!(rig.broker.calls().is_empty());
    let read = pending(7, 1, "Read", json!({"file_path": "/bot/AGENTS.md"}));
    assert_eq!(port.ended(&read).await, Ok(true));
    for (name, input, want) in [
        ("mcp__relay__a_b_c", json!({}), "invalid"),
        ("mcp__relay__agenda_list", json!([1]), "invalid"),
        ("mcp__relay__schedule_deliver", json!({}), "unknown_tool"),
    ] {
        let refused = pending(7, 2, name, input);
        assert_eq!(port.ended(&refused).await, Ok(true), "{name}");
        let recalled = port.recall(&refused).await.unwrap().unwrap();
        assert_eq!(recalled.content[0]["text"].clone(), json!(want), "{name}");
        let Slot::Result(settled) = port.settle(&refused).await.unwrap() else {
            panic!("{name}")
        };
        assert_eq!(settled.content[0]["text"].clone(), json!(want), "{name}");
    }
    assert!(
        rig.broker.calls().is_empty(),
        "front-refused calls must not reach the broker"
    );
    let unrecorded = pending(7, 3, "mcp__relay__agenda_other", json!({}));
    assert_eq!(port.ended(&unrecorded).await, Ok(false));
    rig.ledger
        .down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(port.ended(&slot).await, Err(SessionError::Unavailable));
}

#[tokio::test]
async fn a_confirmed_refusal_mark_closes_the_slot_with_the_same_word_and_never_executes() {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let slot = pending(7, 0, "mcp__relay__agenda_list", json!({"a": 1}));
    let mark = |slot: &Pending, value: serde_json::Value| {
        lock(&rig.ledger.steps).insert(
            bot_core::cli::CliRefusal::key(&CallId::cli(slot.turn, slot.pos)),
            JsonPayload(value.to_string()),
        );
    };
    let text = |out: ToolOutput| (out.content[0]["text"].clone(), out.is_error);
    assert_eq!(port.ended(&slot).await, Ok(false));
    assert_eq!(port.recall(&slot).await, Ok(None));
    for (pos, word) in (1..).zip([
        "unknown_tool",
        "invalid",
        "denied",
        "conflict",
        "stale",
        "unavailable",
    ]) {
        let slot = pending(7, pos, "mcp__relay__agenda_list", json!({"a": 1}));
        mark(&slot, json!(word));
        assert_eq!(port.ended(&slot).await, Ok(true), "{word}");
        assert_eq!(
            text(port.recall(&slot).await.unwrap().unwrap()),
            (json!(word), true)
        );
        assert_eq!(
            text(output(port.settle(&slot).await.unwrap())),
            (json!(word), true)
        );
    }
    assert!(
        rig.broker.calls().is_empty(),
        "marked refusals must not reach the broker"
    );
    for junk in [json!("secret_missing"), json!({"invalid": null})] {
        mark(&slot, junk.clone());
        for error in [
            port.ended(&slot).await.err(),
            port.recall(&slot).await.err(),
            port.settle(&slot).await.err(),
        ] {
            assert_eq!(
                error,
                Some(SessionError::Permanent("corrupt_checkpoint")),
                "{junk}"
            );
        }
    }
    mark(&slot, json!("invalid"));
    let record = |phase| CallRecord {
        call: CallId::cli(7, 0),
        tool: ToolId("agenda.list".into()),
        args_sha256: [0; 32],
        phase,
    };
    for phase in [
        CallPhase::Started {
            generation: Generation(1),
            owner: "o".into(),
            confirmed_step: false,
        },
        CallPhase::Held {
            reason: "timeout".into(),
            resolution: None,
        },
    ] {
        lock(&rig.ledger.calls).insert("cli-7-0".into(), record(phase));
        assert_eq!(port.ended(&slot).await, Ok(false));
        assert_eq!(port.recall(&slot).await, Ok(None));
    }
    lock(&rig.ledger.calls).insert(
        "cli-7-0".into(),
        record(CallPhase::Done(JsonPayload("{}".into()))),
    );
    assert_eq!(port.ended(&slot).await, Ok(true));
    lock(&rig.ledger.calls).insert("cli-7-0".into(), record(CallPhase::Open { approval: None }));
    assert_eq!(
        text(port.recall(&slot).await.unwrap().unwrap()),
        (json!("invalid"), true)
    );
    let unmarked = pending(8, 0, "mcp__relay__agenda_list", json!({}));
    rig.ledger
        .down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(port.ended(&unmarked).await, Err(SessionError::Unavailable));
    assert_eq!(port.recall(&unmarked).await, Err(SessionError::Unavailable));
}

#[tokio::test]
async fn recalling_a_delegate_reads_the_stores_finished_result_without_delegating_again() {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let slot = pending(
        7,
        0,
        "mcp__relay__delegate",
        json!({"agent": "main/research", "instruction": "investigate"}),
    );
    assert_eq!(port.recall(&slot).await, Ok(None));
    assert!(lock(&rig.delegation.requests).is_empty());
    for (body, text, is_error) in [
        (
            json!({"state": "succeeded", "result": "reply"}),
            "reply",
            false,
        ),
        (
            json!({"state": "succeeded", "result": {"a": 1}}),
            r#"{"a":1}"#,
            false,
        ),
        (
            json!({"state": "cancelled", "error": "cancelled"}),
            "child_cancelled",
            true,
        ),
        (json!({"state": "dead", "error": "x"}), "child_failed", true),
    ] {
        lock(&rig.delegation.results).insert("cli-7-0".into(), JsonPayload(body.to_string()));
        let out = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(
            (out.content[0]["text"].clone(), out.is_error),
            (json!(text), is_error)
        );
    }
    let read = lock(&rig.delegation.reads).last().cloned().unwrap();
    assert_eq!(
        (
            read.step.as_str(),
            read.child.0.as_str(),
            read.checked_revision.0.as_str()
        ),
        ("cli-7-0", "main/research", "r1")
    );
    assert!(read.scopes.contains("run"));
    *lock(&rig.delegation.read_error) = Some(Error::Denied);
    let denied = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(denied.content[0]["text"], "denied");
    *lock(&rig.delegation.read_error) = Some(Error::Stale);
    assert_eq!(port.recall(&slot).await, Err(SessionError::Lost));
    let reads = lock(&rig.delegation.reads).len();
    let mut changed = snapshot();
    changed.agents.remove(&AgentId("main/research".into()));
    *lock(&rig.catalog.current) = Arc::new(changed);
    let denied = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(denied.content[0]["text"], "denied");
    assert_eq!(lock(&rig.delegation.reads).len(), reads);
}

#[tokio::test]
async fn settling_a_slot_calls_the_same_call_id_with_canonical_args_and_maps_the_outcome() {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let list = pending(7, 1, "mcp__relay__agenda_list", json!({"b": 1, "a": 2}));
    rig.broker.script(done(
        r#"{"content":[{"type":"text","text":"No events"}],"isError":false}"#,
    ));
    let out = output(port.settle(&list).await.unwrap());
    assert_eq!(out.content, vec![json!({"type":"text","text":"No events"})]);
    assert!(!out.is_error);
    let sent = rig.broker.calls().pop().unwrap();
    assert_eq!(sent.call.0, "cli-7-1", "must match live relay call ID");
    assert_eq!(sent.tool.0, "agenda.list");
    assert_eq!(sent.args.0, r#"{"a":2,"b":1}"#, "canonical sorted JSON");

    rig.broker.script(done(r#"{"events":[]}"#));
    let plain = output(port.settle(&list).await.unwrap());
    assert_eq!(plain.content[0]["text"], r#"{"events":[]}"#);
    rig.broker.script(Err(Error::Denied));
    let denied = output(port.settle(&list).await.unwrap());
    assert_eq!(
        (denied.content[0]["text"].clone(), denied.is_error),
        (json!("denied"), true)
    );
    rig.broker
        .script(Ok(CallResult::Awaiting(ApprovalId("n1".into()))));
    assert_eq!(port.settle(&list).await, Ok(Slot::Parked));
    rig.broker.script(Err(Error::Unavailable));
    assert_eq!(port.settle(&list).await, Err(SessionError::Unavailable));
    let calls_before = rig.broker.calls().len();
    let unknown = pending(7, 2, "mcp__relay__no_such_tool_x", json!({}));
    assert_eq!(
        output(port.settle(&unknown).await.unwrap()).content[0]["text"],
        "invalid"
    );
    let builtin = pending(7, 3, "Read", json!({"file_path": "/bot/AGENTS.md"}));
    let aborted = output(port.settle(&builtin).await.unwrap());
    assert_eq!(
        (aborted.content[0]["text"].clone(), aborted.is_error),
        (json!("aborted"), true)
    );
    assert_eq!(
        rig.broker.calls().len(),
        calls_before,
        "VM built-ins must not call the broker"
    );
}

#[tokio::test]
async fn a_delegate_slot_reuses_the_finished_child_or_delegates_again_under_the_call_id() {
    let input = json!({"agent": "main/research", "instruction": "investigate"});
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let slot = pending(7, 0, "mcp__relay__delegate", input.clone());
    assert_eq!(port.settle(&slot).await, Ok(Slot::Parked));
    let delegated = lock(&rig.delegation.requests).clone();
    assert_eq!(delegated.len(), 1);
    assert_eq!(delegated[0].step, "cli-7-0");
    assert_eq!(delegated[0].child.0, "main/research");
    assert_eq!(delegated[0].checked_revision.0, "r1");

    let mut ctx = agent_run("r2");
    ctx.work.steps.push(Step {
        key: "cli-7-0".into(),
        body: JsonPayload(json!({"state": "succeeded", "result": {"summary": "done"}}).to_string()),
    });
    let (rig, session) = session_of(ctx, true).await;
    lock(&rig.delegation.results).insert(
        "cli-7-0".into(),
        JsonPayload(json!({"state": "succeeded", "result": {"summary": "done"}}).to_string()),
    );
    let port = session.unwrap().0;
    let out = output(port.settle(&slot).await.unwrap());
    assert_eq!(out.content[0]["text"], r#"{"summary":"done"}"#);
    assert!(lock(&rig.delegation.requests).is_empty());
}

#[tokio::test]
async fn a_finished_child_is_returned_only_while_the_store_and_both_revisions_allow_it() {
    let mut ctx = agent_run("r1");
    ctx.work.steps.push(Step {
        key: "cli-7-0".into(),
        body: JsonPayload(json!({"state": "succeeded", "result": "reply"}).to_string()),
    });
    let (rig, session) = session_of(ctx, true).await;
    lock(&rig.delegation.results).insert(
        "cli-7-0".into(),
        JsonPayload(json!({"state": "succeeded", "result": "reply"}).to_string()),
    );
    let port = session.unwrap().0;
    let slot = pending(
        7,
        0,
        "mcp__relay__delegate",
        json!({"agent": "main/research", "instruction": "investigate"}),
    );
    let text = |slot: Slot| output(slot).content[0]["text"].clone();
    assert_eq!(text(port.settle(&slot).await.unwrap()), json!("reply"));

    *lock(&rig.delegation.read_error) = Some(Error::Denied);
    assert_eq!(text(port.settle(&slot).await.unwrap()), json!("denied"));
    *lock(&rig.delegation.read_error) = Some(Error::Stale);
    assert_eq!(port.settle(&slot).await, Err(SessionError::Lost));
    let reads = lock(&rig.delegation.reads).len();
    let mut changed = snapshot();
    changed.agents.remove(&AgentId("main/research".into()));
    *lock(&rig.catalog.current) = Arc::new(changed);
    assert_eq!(text(port.settle(&slot).await.unwrap()), json!("denied"));
    assert_eq!(lock(&rig.delegation.reads).len(), reads);
    assert!(lock(&rig.delegation.requests).is_empty());
}
