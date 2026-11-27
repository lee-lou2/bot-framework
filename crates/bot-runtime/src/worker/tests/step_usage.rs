//! Step-runner token-usage aggregation and opaque provider-state checkpointing tests.

use super::fakes::*;
use crate::{
    model::{CallEnd, DoneCall, Exchange, ModelError, ModelStep, PendingCall, Usage},
    worker::{
        Outcome,
        checkpoint::{Planned, TurnUsage},
    },
};
use bot_core::{command::*, id::*, state::RunContext};

fn final_reply(usage: Option<(u64, u64)>) -> Result<ModelStep, ModelError> {
    Ok(ModelStep::Final {
        text: "reply".into(),
        usage: Usage::default(),
        outcome: RunOutcome {
            truncated: false,
            usage: usage.map(|(prompt, completion)| TokenUsage { prompt, completion }),
        },
    })
}

fn turn(
    call: &str,
    measured: Option<(u64, u64)>,
    state: Option<&str>,
) -> Result<ModelStep, ModelError> {
    let Ok(ModelStep::Calls {
        text, calls, usage, ..
    }) = calls("Checking now", &[(call, "agenda.list", "{}")])
    else {
        unreachable!()
    };
    Ok(ModelStep::Calls {
        text,
        calls,
        usage,
        measured: measured.map(|(prompt, completion)| TokenUsage { prompt, completion }),
        state: state.map(str::to_owned),
    })
}

fn total(prompt: u64, completion: u64) -> RunOutcome {
    RunOutcome {
        truncated: false,
        usage: Some(TokenUsage { prompt, completion }),
    }
}

async fn run(script: Vec<Result<ModelStep, ModelError>>, ctx: RunContext) -> Vec<RunOutcome> {
    let r = rig(ScriptedModel::new(script));
    for _ in 0..4 {
        r.broker.script(done("{}"));
    }
    assert_eq!(r.worker.execute(ctx).await, Outcome::Succeeded);
    lock(&r.runs.state).outcomes.clone()
}

#[tokio::test]
async fn the_total_is_the_sum_of_every_request_when_all_of_them_were_reported() {
    let outcomes = run(
        vec![
            turn("r1:0:0", Some((5, 2)), None),
            final_reply(Some((9, 4))),
        ],
        agent_run("r1"),
    )
    .await;
    assert_eq!(outcomes, vec![total(14, 6)]);
    let outcomes = run(
        vec![
            turn("r2:0:0", Some((5, 2)), None),
            turn("r2:1:0", Some((7, 3)), None),
            final_reply(Some((11, 1))),
        ],
        agent_run("r2"),
    )
    .await;
    assert_eq!(outcomes, vec![total(23, 6)]);
}

#[tokio::test]
async fn one_unreported_request_leaves_the_total_unknown_whichever_turn_it_was() {
    let first = run(
        vec![turn("r1:0:0", None, None), final_reply(Some((9, 4)))],
        agent_run("r1"),
    )
    .await;
    assert_eq!(first, vec![RunOutcome::default()]);
    let last = run(
        vec![turn("r2:0:0", Some((5, 2)), None), final_reply(None)],
        agent_run("r2"),
    )
    .await;
    assert_eq!(last, vec![RunOutcome::default()]);
}

#[tokio::test]
async fn a_re_claimed_run_has_no_total_because_a_lost_request_may_have_been_billed() {
    let retaken = run(
        vec![
            turn("r1:0:0", Some((5, 2)), None),
            final_reply(Some((9, 4))),
        ],
        generation(agent_run("r1"), 2),
    )
    .await;
    assert_eq!(retaken, vec![RunOutcome::default()]);
}

fn resumed(run: &str, usage: TurnUsage, delegation: Delegation) -> RunContext {
    let call = CallId(format!("{run}:0:0"));
    let delegate = delegation != Delegation::None;
    let tool = ToolId(if delegate { "delegate" } else { "agenda.list" }.into());
    let planned = Planned {
        text: "Checking now".into(),
        calls: vec![PendingCall {
            call: call.clone(),
            tool: tool.clone(),
            args: JsonPayload("{}".into()),
        }],
        usage,
        state: None,
    };
    let finished = Exchange {
        text: "Checking now".into(),
        calls: vec![DoneCall {
            call,
            tool,
            args: JsonPayload("{}".into()),
            end: CallEnd::Done(JsonPayload("{}".into())),
        }],
        state: None,
    };
    let mut ctx = agent_run(run);
    ctx.work.steps = vec![
        Step {
            key: "turn-0".into(),
            body: planned.to_payload(),
        },
        Step {
            key: "end-0".into(),
            body: finished.to_payload(),
        },
    ];
    if delegation == Delegation::Child {
        ctx.work.steps.push(Step {
            key: format!("{run}:0:0"),
            body: JsonPayload(r#"{"state":"succeeded","result":{}}"#.into()),
        });
    }
    ctx
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Delegation {
    None,
    NoChild,
    Child,
}

#[tokio::test]
async fn a_resumed_run_counts_the_checkpointed_turns_and_only_a_recorded_report_is_a_basis() {
    let reported = TurnUsage::Reported(TokenUsage {
        prompt: 5,
        completion: 2,
    });
    let outcomes = run(
        vec![final_reply(Some((9, 4)))],
        resumed("r1", reported, Delegation::None),
    )
    .await;
    assert_eq!(outcomes, vec![total(14, 6)]);
    for usage in [TurnUsage::Unreported, TurnUsage::Unknown] {
        let outcomes = run(
            vec![final_reply(Some((9, 4)))],
            resumed("r2", usage, Delegation::None),
        )
        .await;
        assert_eq!(outcomes, vec![RunOutcome::default()], "{usage:?}");
    }
    let outcomes = run(
        vec![final_reply(Some((9, 4)))],
        resumed("r3", reported, Delegation::Child),
    )
    .await;
    assert_eq!(outcomes, vec![RunOutcome::default()]);
    let outcomes = run(
        vec![final_reply(Some((9, 4)))],
        resumed("r4", reported, Delegation::NoChild),
    )
    .await;
    assert_eq!(outcomes, vec![total(14, 6)]);
}

#[tokio::test]
async fn the_provider_state_is_checkpointed_with_the_turn_and_handed_back_on_the_next_request() {
    let model = ScriptedModel::new(vec![
        turn("r1:0:0", Some((5, 2)), Some(r#"{"v":1,"k":"openai"}"#)),
        final_reply(Some((9, 4))),
    ]);
    let r = rig(model.clone());
    r.broker.script(done("{}"));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    let steps = lock(&r.runs.state).steps.clone();
    let body = |key: &str| {
        let step = steps.iter().find(|s| s.key == key).expect(key);
        serde_json::from_str::<serde_json::Value>(&step.body.0).unwrap()
    };
    assert_eq!(body("turn-0")["state"], r#"{"v":1,"k":"openai"}"#);
    assert_eq!(
        body("turn-0")["usage"],
        serde_json::json!({"prompt": 5, "completion": 2})
    );
    assert_eq!(body("end-0")["state"], r#"{"v":1,"k":"openai"}"#);
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].exchanges.len(), 0);
    assert_eq!(
        requests[1].exchanges[0].state.as_deref(),
        Some(r#"{"v":1,"k":"openai"}"#)
    );
}

#[tokio::test]
async fn an_oversized_provider_state_is_refused_before_anything_is_checkpointed_or_called() {
    let huge = "s".repeat(300 * 1024);
    let r = rig(ScriptedModel::new(vec![turn(
        "r1:0:0",
        None,
        Some(huge.as_str()),
    )]));
    r.broker.script(done("{}"));
    assert!(matches!(
        r.worker.execute(agent_run("r1")).await,
        Outcome::Failed(_)
    ));
    let state = lock(&r.runs.state);
    assert_eq!(state.failed, [Failure::Permanent("invalid_model_output")]);
    assert!(state.steps.is_empty(), "plan must not be checkpointed");
    assert!(r.broker.calls().is_empty(), "tool must not be called");
}
