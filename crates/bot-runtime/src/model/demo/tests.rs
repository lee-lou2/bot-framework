//! DemoModel tests covering deterministic agenda listing, deletion, simulation, and cancellation.

use super::*;
use crate::model::{Exchange, Limits, Message, ToolKind, ToolSpec};
use bot_core::{id::*, state::Risk};
use tokio::sync::watch;

fn tool(id: &str) -> ToolSpec {
    ToolSpec {
        id: ToolId(id.into()),
        kind: ToolKind::Skill,
        class: Risk::Read,
        description: String::new(),
        input: JsonPayload("{}".into()),
    }
}

fn request(text: &str, tools: &[&str], exchanges: Vec<Exchange>) -> ModelRequest {
    ModelRequest {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(1),
        },
        model: "assistant".into(),
        system: String::new(),
        conversation: vec![
            Message {
                role: Role::User,
                speaker: None,
                text: "previous user turn".into(),
            },
            Message {
                role: Role::Assistant,
                speaker: None,
                text: "previous reply".into(),
            },
            Message {
                role: Role::User,
                speaker: Some(PrincipalId("local:owner".into())),
                text: text.into(),
            },
        ],
        tools: tools.iter().map(|t| tool(t)).collect(),
        exchanges,
        limits: Limits::default(),
        cancel: watch::channel(false).1,
        builtin: Default::default(),
        relay: None,
        session: None,
    }
}

const BOTH: [&str; 2] = [LIST, DELETE];

async fn step(request: ModelRequest) -> ModelStep {
    DemoModel.step(request).await.unwrap()
}

fn done(tool: &str, args: &str, end: CallEnd) -> Exchange {
    Exchange {
        text: String::new(),
        calls: vec![DoneCall {
            call: call_id(&RunId("r1".into()), 0, 0),
            tool: ToolId(tool.into()),
            args: JsonPayload(args.into()),
            end,
        }],
        state: None,
    }
}

fn listed(result: &str) -> Exchange {
    done(LIST, "{}", CallEnd::Done(JsonPayload(result.into())))
}

const EVENTS: &str = r#"{"events":[{"id":"e-walk","title":"Walk","start":"07:30"},{"id":"e-meet","title":"Team meeting","start":"14:00"}]}"#;

fn calls_of(step: &ModelStep) -> Vec<(String, String, String)> {
    match step {
        ModelStep::Calls { calls, .. } => calls
            .iter()
            .map(|c| (c.call.0.clone(), c.tool.0.clone(), c.args.0.clone()))
            .collect(),
        other => panic!("expected tool calls: {other:?}"),
    }
}

fn text_of(step: &ModelStep) -> &str {
    match step {
        ModelStep::Final { text, .. } => text,
        other => panic!("expected final reply: {other:?}"),
    }
}

#[tokio::test]
async fn listing_asks_for_the_agenda_then_summarizes_it_in_korean() {
    let first = step(request("Show today's agenda", &BOTH, vec![])).await;
    assert_eq!(
        calls_of(&first),
        [("r1:0:0".into(), "agenda.list".into(), "{}".into())]
    );
    let second = step(request("Show today's agenda", &BOTH, vec![listed(EVENTS)])).await;
    assert_eq!(
        text_of(&second),
        "Found 2 event(s).\n- 07:30 Walk\n- 14:00 Team meeting"
    );
}

#[tokio::test]
async fn understands_common_tool_result_shapes() {
    for (shape, want) in [
        (
            r#"[{"id":1,"name":"Workout"}]"#,
            "Found 1 event(s).\n- Workout",
        ),
        (
            r#"{"structuredContent":{"events":[{"id":"a","title":"Lunch","time":"12:00"}]}}"#,
            "Found 1 event(s).\n- 12:00 Lunch",
        ),
        (
            r#"{"content":[{"type":"text","text":"{\"events\":[{\"id\":\"a\",\"summary\":\"Reading\"}]}"}]}"#,
            "Found 1 event(s).\n- Reading",
        ),
        (r#"{"events":[]}"#, "No events found."),
        (r#"[]"#, "No events found."),
        (
            r#"{"content":[{"type":"text","text":"No events scheduled today"}]}"#,
            "No events scheduled today",
        ),
        ("not json at all", "not json at all"),
    ] {
        let reply = step(request("Show schedule", &BOTH, vec![listed(shape)])).await;
        assert_eq!(text_of(&reply), want, "{shape}");
    }
}

#[tokio::test]
async fn listing_failures_are_reported_plainly() {
    let denied = done(LIST, "{}", CallEnd::Denied);
    assert_eq!(
        text_of(&step(request("Show agenda", &BOTH, vec![denied])).await),
        "Permission denied to read the agenda."
    );
    let failed = done(LIST, "{}", CallEnd::Failed("unavailable".into()));
    assert!(
        text_of(&step(request("Show agenda", &BOTH, vec![failed])).await)
            .starts_with("Could not fetch the agenda")
    );
}

#[tokio::test]
async fn deleting_finds_the_event_by_title_then_deletes_it_by_id() {
    let text = "Delete the walk event";
    let first = step(request(text, &BOTH, vec![])).await;
    assert_eq!(
        calls_of(&first),
        [("r1:0:0".into(), "agenda.list".into(), "{}".into())]
    );

    let second = step(request(text, &BOTH, vec![listed(EVENTS)])).await;
    assert_eq!(
        calls_of(&second),
        [(
            "r1:1:0".into(),
            "agenda.delete".into(),
            r#"{"event":"e-walk"}"#.into()
        )]
    );

    let deleted = done(
        DELETE,
        r#"{"event":"e-walk"}"#,
        CallEnd::Done(JsonPayload("{}".into())),
    );
    let third = step(request(text, &BOTH, vec![listed(EVENTS), deleted])).await;
    assert_eq!(text_of(&third), "Deleted 'Walk'.");

    // Simulated delete responses (raw or MCP-wrapped) must not claim permanent deletion.
    let sim = r#"{"simulated":true,"persisted":false,"receipt":"sim-7","event":"e-walk"}"#;
    let wrapped = serde_json::json!({"content": [{"type": "text", "text": sim}], "isError": false});
    for payload in [sim.to_owned(), wrapped.to_string()] {
        let simulated = done(
            DELETE,
            r#"{"event":"e-walk"}"#,
            CallEnd::Done(JsonPayload(payload)),
        );
        let said = step(request(text, &BOTH, vec![listed(EVENTS), simulated])).await;
        assert_eq!(
            text_of(&said),
            "Simulated deletion of 'Walk' (virtual receipt sim-7)."
        );
    }
    let no_receipt = done(
        DELETE,
        r#"{"event":"e-walk"}"#,
        CallEnd::Done(JsonPayload(r#"{"simulated":true}"#.into())),
    );
    let said = step(request(text, &BOTH, vec![listed(EVENTS), no_receipt])).await;
    assert_eq!(
        text_of(&said),
        "Simulated deletion of 'Walk' (not permanently deleted)."
    );
    let not_simulated = done(
        DELETE,
        r#"{"event":"e-walk"}"#,
        CallEnd::Done(JsonPayload(r#"{"simulated":false}"#.into())),
    );
    let said = step(request(text, &BOTH, vec![listed(EVENTS), not_simulated])).await;
    assert_eq!(text_of(&said), "Deleted 'Walk'.");

    let denied = done(DELETE, r#"{"event":"e-walk"}"#, CallEnd::Denied);
    let refused = step(request(text, &BOTH, vec![listed(EVENTS), denied])).await;
    assert_eq!(
        text_of(&refused),
        "Deletion was not approved, so 'Walk' was kept."
    );

    let failed = done(
        DELETE,
        r#"{"event":"e-walk"}"#,
        CallEnd::Failed("unavailable".into()),
    );
    let broken = step(request(text, &BOTH, vec![listed(EVENTS), failed])).await;
    assert!(text_of(&broken).starts_with("Could not delete the event"));
}

#[tokio::test]
async fn deleting_never_guesses_when_the_event_is_missing_or_ambiguous() {
    let none = step(request("Delete dinner event", &BOTH, vec![listed(EVENTS)])).await;
    assert!(
        text_of(&none).starts_with("Could not find that event.\nFound 2 event(s)."),
        "{}",
        text_of(&none)
    );
    let many = r#"[{"id":"1","title":"Workout"},{"id":"2","title":"Lunch"}]"#;
    let ambiguous = step(request(
        "Remove workout and lunch",
        &BOTH,
        vec![listed(many)],
    ))
    .await;
    assert_eq!(
        text_of(&ambiguous),
        "Multiple events match: Workout, Lunch. Which event should I delete?"
    );
    let nested = r#"[{"id":"1","title":"Walk"},{"id":"2","title":"Evening Walk"}]"#;
    let pick = step(request("Delete Evening Walk", &BOTH, vec![listed(nested)])).await;
    assert_eq!(calls_of(&pick)[0].2, r#"{"event":"2"}"#);
}

#[tokio::test]
async fn missing_tools_and_unknown_requests_get_a_plain_answer() {
    assert!(
        text_of(&step(request("Show agenda", &[], vec![])).await)
            .starts_with("Agenda tools are unavailable")
    );
    let no_delete = step(request(
        "Delete the walk event",
        &[LIST],
        vec![listed(EVENTS)],
    ))
    .await;
    assert!(text_of(&no_delete).starts_with("Delete tool is unavailable"));
    assert_eq!(text_of(&step(request("hello", &BOTH, vec![])).await), HELP);
}

#[tokio::test]
async fn the_demo_model_is_a_step_runner_that_never_needs_a_relay() {
    assert_eq!(DemoModel.kind(), RunnerKind::Step);
    let with_relay = request("Show agenda", &BOTH, vec![]);
    assert!(with_relay.relay.is_none());
}

#[tokio::test]
async fn the_same_request_always_gives_the_same_step_and_cancel_is_honored() {
    let a = step(request(
        "Delete the walk event",
        &BOTH,
        vec![listed(EVENTS)],
    ))
    .await;
    let b = step(request(
        "Delete the walk event",
        &BOTH,
        vec![listed(EVENTS)],
    ))
    .await;
    assert_eq!(a, b);
    let (tx, rx) = watch::channel(false);
    tx.send(true).unwrap();
    let mut cancelled = request("Show agenda", &BOTH, vec![]);
    cancelled.cancel = rx;
    assert_eq!(DemoModel.step(cancelled).await, Err(ModelError::Cancelled));
}
