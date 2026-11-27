//! Tests for replay precedence between stored CLI refusal marks and existing delegation children.

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
use bot_core::{cli::CliRefusal, command::*, id::*, port::Error, state::RunContext};
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

async fn session_of(ctx: RunContext) -> (Rig, Session) {
    let grabbed = Arc::new(Mutex::new(None));
    let model: Arc<dyn ModelRunner> = Arc::new(Grab(grabbed.clone()));
    let rig = rig_with_relays(model, Arc::new(Open));
    assert_eq!(rig.worker.execute(ctx).await, Outcome::Succeeded);
    let session = lock(&grabbed)
        .take()
        .expect("session port should exist when relay is open");
    (rig, session)
}

fn delegate(input: serde_json::Value) -> Pending {
    Pending {
        turn: 7,
        pos: 0,
        tool_use: ToolUse {
            id: "toolu_7_0".into(),
            name: "mcp__relay__delegate".into(),
            input,
        },
    }
}

fn valid() -> serde_json::Value {
    json!({"agent": "main/research", "instruction": "investigate"})
}

fn mark(rig: &Rig, word: &str) {
    lock(&rig.ledger.steps).insert(
        CliRefusal::key(&CallId::cli(7, 0)),
        JsonPayload(json!(word).to_string()),
    );
}

fn text(out: &ToolOutput) -> (serde_json::Value, bool) {
    (out.content[0]["text"].clone(), out.is_error)
}

fn closed(slot: Slot) -> ToolOutput {
    match slot {
        Slot::Result(output) => output,
        Slot::Parked => panic!("expected Slot::Result"),
    }
}

fn words() -> Vec<&'static str> {
    [
        "unknown_tool",
        "invalid",
        "denied",
        "conflict",
        "stale",
        "unavailable",
        "secret_missing",
        "destination_unsupported",
        "destination_unregistered",
        "scope_denied",
        "rate_limited",
    ]
    .into_iter()
    .filter(|w| CliRefusal::try_from((*w).to_owned()).is_ok())
    .collect()
}

#[tokio::test]
async fn a_front_refusal_on_a_delegate_is_replayed_without_creating_a_child_even_after_the_permission_returns()
 {
    let (rig, session) = session_of(agent_run("r1")).await;
    let port = session.0;
    let slot = delegate(valid());
    assert!(words().len() >= 6, "{:?}", words());
    for word in words() {
        mark(&rig, word);
        assert_eq!(port.ended(&slot).await, Ok(true), "{word}");
        let recalled = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(text(&recalled), (json!(word), true), "{word}");
        let settled = closed(port.settle(&slot).await.unwrap());
        assert_eq!(text(&settled), (json!(word), true), "{word}");

        let reads = lock(&rig.delegation.reads).len();
        let mut changed = snapshot();
        changed.agents.remove(&AgentId("main/research".into()));
        *lock(&rig.catalog.current) = Arc::new(changed);
        let lost = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(text(&lost), (json!(word), true), "{word}: permission lost");
        assert_eq!(port.ended(&slot).await, Ok(true), "{word}");
        assert_eq!(lock(&rig.delegation.reads).len(), reads);

        *lock(&rig.catalog.current) = rig.catalog.pinned.clone();
        let again = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(
            text(&again),
            (json!(word), true),
            "{word}: permission restored"
        );
        let settled = closed(port.settle(&slot).await.unwrap());
        assert_eq!(
            text(&settled),
            (json!(word), true),
            "{word}: permission restored"
        );

        let broken = delegate(json!({"agent": 5}));
        let out = port.recall(&broken).await.unwrap().unwrap();
        assert_eq!(text(&out), (json!(word), true), "{word}: invalid input");
    }
    assert!(
        lock(&rig.delegation.requests).is_empty(),
        "refused delegate must not create a child"
    );
    assert!(rig.broker.calls().is_empty());
}

#[tokio::test]
async fn an_existing_child_or_a_store_mismatch_wins_over_the_mark() {
    let (rig, session) = session_of(agent_run("r1")).await;
    let port = session.0;
    let slot = delegate(valid());
    mark(&rig, "conflict");
    lock(&rig.delegation.results).insert(
        "cli-7-0".into(),
        JsonPayload(json!({"state": "succeeded", "result": "reply"}).to_string()),
    );
    let out = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(text(&out), (json!("reply"), false));
    assert_eq!(port.ended(&slot).await, Ok(true));
    lock(&rig.delegation.results).clear();
    *lock(&rig.delegation.read_error) = Some(Error::Denied);
    let out = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(text(&out), (json!("denied"), true));
    *lock(&rig.delegation.read_error) = Some(Error::Stale);
    assert_eq!(port.recall(&slot).await, Err(SessionError::Lost));
}

#[tokio::test]
async fn only_an_absent_mark_allows_the_delegate_to_be_judged_again() {
    let (rig, session) = session_of(agent_run("r1")).await;
    let port = session.0;
    let slot = delegate(valid());
    for junk in ["\"nope\"", "{\"invalid\":null}"] {
        lock(&rig.ledger.steps).insert(
            CliRefusal::key(&CallId::cli(7, 0)),
            JsonPayload(junk.into()),
        );
        let error = SessionError::Permanent("corrupt_checkpoint");
        assert_eq!(port.ended(&slot).await, Err(error), "{junk}");
        assert_eq!(port.recall(&slot).await, Err(error), "{junk}");
        assert_eq!(port.settle(&slot).await.err(), Some(error), "{junk}");
    }
    assert!(lock(&rig.delegation.requests).is_empty());
    lock(&rig.ledger.steps).clear();
    assert_eq!(port.ended(&slot).await, Ok(false));
    assert_eq!(port.recall(&slot).await, Ok(None));
    assert!(matches!(port.settle(&slot).await, Ok(Slot::Parked)));
    assert_eq!(lock(&rig.delegation.requests).len(), 1);
    let mut changed = snapshot();
    changed.agents.remove(&AgentId("main/research".into()));
    *lock(&rig.catalog.current) = Arc::new(changed);
    lock(&rig.ledger.steps).clear();
    let out = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(text(&out), (json!("denied"), true));
}
