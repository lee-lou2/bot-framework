//! Agent run Step/Loop tests: checkpoint ordering, resume, approvals, delegation, memory, and limits.

use super::fakes::*;
use crate::{
    host::Models,
    model::{
        CallEnd, ModelError, ModelRequest, ModelRunner, ModelStep, PendingCall, RunnerKind,
        StepFuture,
    },
    worker::{
        Outcome,
        checkpoint::{Planned, TurnUsage},
    },
};
use bot_core::{command::*, id::*, port::Error, state::Visibility};
use std::sync::Arc;

struct NoModels;

impl Models for NoModels {
    fn runner(&self, _: &ProfileId, _: &str) -> Option<Arc<dyn ModelRunner>> {
        None
    }
}

fn plan(run: &str, calls: &[(&str, &str, &str)]) -> Step {
    let planned = Planned {
        text: "Looking it up".into(),
        calls: calls
            .iter()
            .map(|(call, tool, args)| PendingCall {
                call: CallId(format!("{run}:{call}")),
                tool: ToolId((*tool).into()),
                args: JsonPayload((*args).into()),
            })
            .collect(),
        usage: TurnUsage::Unreported,
        state: None,
    };
    Step {
        key: "turn-0".into(),
        body: planned.to_payload(),
    }
}

fn answer(text: &str) -> JsonPayload {
    JsonPayload(serde_json::json!({ "text": text }).to_string())
}

#[tokio::test]
async fn a_plain_answer_finishes_with_the_text_as_result_and_reply() {
    let model = ScriptedModel::new(vec![say("No events today")]);
    let r = rig(model.clone());
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    let state = lock(&r.runs.state);
    assert_eq!(state.finished.len(), 1);
    assert_eq!(state.finished[0].result, answer("No events today"));
    assert_eq!(state.finished[0].answers, vec![answer("No events today")]);
    assert!(state.steps.is_empty() && state.failed.is_empty());
    let request = &model.requests()[0];
    assert_eq!(request.attempt.run.0, "r1");
    let expected_instructions = &snapshot().agents[&AgentId("main".into())].instructions;
    assert!(
        request.system.contains(expected_instructions.as_str()),
        "assembled context must include agent instructions"
    );
    assert!(request.tools.iter().any(|t| t.id.0 == "agenda.list"));
}

#[tokio::test]
async fn the_models_turn_is_checkpointed_before_its_calls_run_and_the_next_turn_sees_the_results() {
    let model = ScriptedModel::new(vec![
        calls("Looking it up", &[("r1:0:0", "agenda.list", "{}")]),
        say("No events found."),
    ]);
    let r = rig(model.clone());
    r.broker.script(done(r#"{"events":[]}"#));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    assert_eq!(
        r.runs.events(),
        ["checkpoint:turn-0", "call:agenda.list", "checkpoint:end-0"]
    );
    let requests = model.requests();
    assert_eq!(requests[0].exchanges.len(), 0);
    assert_eq!(requests[1].exchanges.len(), 1);
    assert_eq!(
        requests[1].exchanges[0].calls[0].end,
        CallEnd::Done(JsonPayload(r#"{"events":[]}"#.into()))
    );
    assert_eq!(r.broker.calls()[0].call.0, "r1:0:0");
}

#[tokio::test]
async fn resuming_continues_a_half_done_turn_with_the_same_call_ids_and_skips_finished_turns() {
    let mut ctx = agent_run("r1");
    ctx.work.steps = vec![plan("r1", &[("0:0", "agenda.list", "{}")])];
    let model = ScriptedModel::new(vec![say("done")]);
    let r = rig(model.clone());
    assert_eq!(r.worker.execute(ctx).await, Outcome::Succeeded);
    assert_eq!(r.broker.calls()[0].call.0, "r1:0:0");
    assert_eq!(
        r.runs.events(),
        ["call:agenda.list", "checkpoint:end-0"],
        "must not rewrite existing plan checkpoint"
    );
    assert_eq!(model.requests().len(), 1);
    assert_eq!(model.requests()[0].exchanges.len(), 1);

    let finished = crate::model::Exchange {
        text: "Looking it up".into(),
        calls: vec![],
        state: None,
    };
    let mut ctx = agent_run("r2");
    ctx.work.steps = vec![
        plan("r2", &[("0:0", "agenda.list", "{}")]),
        Step {
            key: "end-0".into(),
            body: finished.to_payload(),
        },
    ];
    let model = ScriptedModel::new(vec![say("immediate reply")]);
    let r = rig(model.clone());
    assert_eq!(r.worker.execute(ctx).await, Outcome::Succeeded);
    assert!(r.broker.calls().is_empty());
    assert_eq!(model.requests()[0].exchanges.len(), 1);
}

#[tokio::test]
async fn a_corrupt_checkpoint_is_not_silently_accepted() {
    let mut ctx = agent_run("r1");
    ctx.work.steps = vec![Step {
        key: "turn-0".into(),
        body: JsonPayload("{}".into()),
    }];
    let r = rig(ScriptedModel::new(vec![say("x")]));
    assert!(matches!(r.worker.execute(ctx).await, Outcome::Failed(_)));
    assert_eq!(
        lock(&r.runs.state).failed,
        [Failure::Permanent("corrupt_checkpoint")]
    );
}

#[tokio::test]
async fn an_approval_wait_parks_the_attempt_without_reporting_anything() {
    let model = ScriptedModel::new(vec![calls(
        "Deleting now",
        &[("r1:0:0", "agenda.delete", "{}")],
    )]);
    let r = rig(model);
    r.broker
        .script(Ok(CallResult::Awaiting(ApprovalId("n1".into()))));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Parked);
    let state = lock(&r.runs.state);
    assert!(state.finished.is_empty() && state.failed.is_empty());
    assert!(state.steps.iter().any(|s| s.key == "turn-0"));
    assert!(
        !state.steps.iter().any(|s| s.key == "end-0"),
        "incomplete turn must not write end-0"
    );
}

#[tokio::test]
async fn a_held_call_also_parks() {
    let model = ScriptedModel::new(vec![calls(
        "Sending now",
        &[("r1:0:0", "agenda.list", "{}")],
    )]);
    let r = rig(model);
    r.broker
        .script(Ok(CallResult::Held(CallId("r1:0:0".into()))));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Parked);
}

#[tokio::test]
async fn a_denied_call_is_told_to_the_model_instead_of_failing_the_run() {
    let model = ScriptedModel::new(vec![
        calls("Deleting now", &[("r1:0:0", "agenda.delete", "{}")]),
        say("Permission denied; could not delete"),
    ]);
    let r = rig(model.clone());
    r.broker.script(Err(Error::Denied));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    assert_eq!(
        model.requests()[1].exchanges[0].calls[0].end,
        CallEnd::Denied
    );
    let model = ScriptedModel::new(vec![
        calls(
            "Trying now",
            &[("r2:0:0", "nope", "{}"), ("r2:0:1", "agenda.list", "{}")],
        ),
        say("Understood"),
    ]);
    let r = rig(model.clone());
    r.broker.script(Err(Error::Missing));
    r.broker.script(Err(Error::Invalid));
    assert_eq!(r.worker.execute(agent_run("r2")).await, Outcome::Succeeded);
    let ends: Vec<_> = model.requests()[1].exchanges[0]
        .calls
        .iter()
        .map(|c| c.end.clone())
        .collect();
    assert_eq!(
        ends,
        [
            CallEnd::Failed("unknown_tool".into()),
            CallEnd::Failed("invalid_args".into())
        ]
    );
}

#[tokio::test]
async fn broker_trouble_fails_the_run_with_a_fixed_code() {
    let r = rig(ScriptedModel::new(vec![calls(
        "Trying now",
        &[("r1:0:0", "agenda.list", "{}")],
    )]));
    r.broker.script(Err(Error::Unavailable));
    assert!(matches!(
        r.worker.execute(agent_run("r1")).await,
        Outcome::Failed(_)
    ));
    assert_eq!(
        lock(&r.runs.state).failed,
        [Failure::Transient("broker_unavailable")]
    );
    let r = rig(ScriptedModel::new(vec![calls(
        "Trying now",
        &[("r2:0:0", "agenda.list", "{}")],
    )]));
    r.broker.script(Err(Error::Conflict));
    assert!(matches!(
        r.worker.execute(agent_run("r2")).await,
        Outcome::Failed(_)
    ));
    assert_eq!(
        lock(&r.runs.state).failed,
        [Failure::Permanent("call_conflict")]
    );
}

#[tokio::test]
async fn a_stale_reply_is_retried_while_the_lease_lives_and_abandoned_once_it_is_lost() {
    let slow = crate::host::WorkerConfig {
        heartbeat: std::time::Duration::from_secs(60),
        ..config()
    };
    let model = ScriptedModel::new(vec![
        calls("Trying now", &[("r1:0:0", "agenda.list", "{}")]),
        say("Done"),
    ]);
    let r = rig_with(model, slow.clone());
    r.broker.script(Err(Error::Stale));
    r.broker.script(done("{}"));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    assert_eq!(
        r.broker.calls().len(),
        2,
        "retried call must reuse the same call ID"
    );
    assert_eq!(r.broker.calls()[0].call, r.broker.calls()[1].call);

    let model = ScriptedModel::new(vec![calls(
        "Trying now",
        &[("r2:0:0", "agenda.list", "{}")],
    )]);
    let r = rig_with(model, slow);
    r.broker.script(Err(Error::Stale));
    r.runs.renew_script(Err(Error::Stale));
    assert_eq!(r.worker.execute(agent_run("r2")).await, Outcome::Lost);
    let state = lock(&r.runs.state);
    assert!(
        state.finished.is_empty() && state.failed.is_empty(),
        "lost claim must not report anything"
    );
}

fn delegate_args(agent: &str) -> String {
    serde_json::json!({"agent": agent, "instruction": "Check schedule"}).to_string()
}

#[tokio::test]
async fn delegation_narrows_the_scopes_parks_the_parent_and_hands_the_child_result_back() {
    let mut ctx = agent_run("r1");
    ctx.scopes = scopes(&["run", "calendar.read", "admin.x"]);
    let args = delegate_args("main/research");
    let model = ScriptedModel::new(vec![calls(
        "Delegating now",
        &[("r1:0:0", "delegate", &args)],
    )]);
    let r = rig(model);
    assert_eq!(r.worker.execute(ctx.clone()).await, Outcome::Parked);
    let sent = lock(&r.delegation.requests).clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (
            sent[0].step.as_str(),
            sent[0].child.0.as_str(),
            sent[0].instruction.as_str()
        ),
        ("r1:0:0", "main/research", "Check schedule")
    );
    assert_eq!(
        sent[0].scopes,
        scopes(&["run", "calendar.read"]),
        "delegated scopes must intersect caller, child, and profile policy"
    );
    assert!(lock(&r.runs.state).finished.is_empty());

    let mut resumed = ctx.clone();
    resumed.work.steps = vec![
        plan("r1", &[("0:0", "delegate", &args)]),
        Step {
            key: "r1:0:0".into(),
            body: JsonPayload(r#"{"state":"succeeded","result":{"text":"No events"}}"#.into()),
        },
    ];
    let model = ScriptedModel::new(vec![say("Research result: no events")]);
    let r = rig(model.clone());
    lock(&r.delegation.results).insert(
        "r1:0:0".into(),
        JsonPayload(r#"{"state":"succeeded","result":{"text":"No events"}}"#.into()),
    );
    assert_eq!(r.worker.execute(resumed).await, Outcome::Succeeded);
    assert!(lock(&r.delegation.requests).is_empty());
    assert_eq!(
        model.requests()[0].exchanges[0].calls[0].end,
        CallEnd::Done(answer("No events"))
    );

    for (body, expected) in [
        (r#"{"state":"dead","error":"x"}"#, "child_failed"),
        (
            r#"{"state":"cancelled","error":"cancelled"}"#,
            "child_cancelled",
        ),
    ] {
        let mut resumed = ctx.clone();
        resumed.work.steps = vec![
            plan("r1", &[("0:0", "delegate", &args)]),
            Step {
                key: "r1:0:0".into(),
                body: JsonPayload(body.into()),
            },
        ];
        let model = ScriptedModel::new(vec![say("Understood")]);
        let r = rig(model.clone());
        lock(&r.delegation.results).insert("r1:0:0".into(), JsonPayload(body.into()));
        assert_eq!(r.worker.execute(resumed).await, Outcome::Succeeded);
        assert_eq!(
            model.requests()[0].exchanges[0].calls[0].end,
            CallEnd::Failed(expected.into())
        );
    }
}

#[tokio::test]
async fn only_a_direct_child_can_be_delegated_to() {
    for agent in ["main", "main/research/deep", "other"] {
        let args = delegate_args(agent);
        let model = ScriptedModel::new(vec![
            calls("Delegating now", &[("r1:0:0", "delegate", &args)]),
            say("Not allowed"),
        ]);
        let r = rig(model.clone());
        assert_eq!(
            r.worker.execute(agent_run("r1")).await,
            Outcome::Succeeded,
            "{agent}"
        );
        assert!(
            lock(&r.delegation.requests).is_empty(),
            "{agent}: must be rejected before reaching store"
        );
        assert_eq!(
            model.requests()[1].exchanges[0].calls[0].end,
            CallEnd::Failed("denied".into())
        );
    }
    let args = delegate_args("main/research");
    let model = ScriptedModel::new(vec![
        calls("Delegating now", &[("r2:0:0", "delegate", &args)]),
        say("Not allowed"),
    ]);
    let r = rig(model.clone());
    *lock(&r.delegation.error) = Some(Error::Denied);
    assert_eq!(r.worker.execute(agent_run("r2")).await, Outcome::Succeeded);
    assert_eq!(
        model.requests()[1].exchanges[0].calls[0].end,
        CallEnd::Failed("denied".into())
    );
}

#[tokio::test]
async fn memory_hits_reach_the_prompt_as_untrusted_reference_and_memory_trouble_does_not_fail_the_run()
 {
    let model = ScriptedModel::new(vec![say("Okay")]);
    let r = rig(model.clone());
    *lock(&r.memory.hits) = vec![MemoryHit {
        id: MemoryId("m1".into()),
        visibility: Visibility::Profile,
        text: "Only drinks low-fat milk".into(),
    }];
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    let request = &model.requests()[0];
    assert!(!request.system.contains("Only drinks low-fat milk"));
    let reference = &request.conversation[request.conversation.len() - 2];
    assert!(reference.text.starts_with(crate::context::REFERENCE_HEADER));
    assert!(reference.text.contains("Only drinks low-fat milk"));

    let model = ScriptedModel::new(vec![say("Okay")]);
    let r = rig(model.clone());
    *lock(&r.memory.fail) = true;
    assert_eq!(r.worker.execute(agent_run("r2")).await, Outcome::Succeeded);
}

#[tokio::test]
async fn previous_turns_of_the_conversation_reach_the_model() {
    let model = ScriptedModel::new(vec![say("Okay")]);
    let r = rig(model.clone());
    lock(&r.runs.state).history = vec![
        Turn {
            from: Some(PrincipalId("local:owner".into())),
            text: "What did I do yesterday?".into(),
        },
        Turn {
            from: None,
            text: "Went for a walk.".into(),
        },
    ];
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    let texts: Vec<_> = model.requests()[0]
        .conversation
        .iter()
        .map(|m| m.text.clone())
        .collect();
    assert_eq!(texts[..2], ["What did I do yesterday?", "Went for a walk."]);
}

#[tokio::test]
async fn failures_carry_only_fixed_codes_and_unknown_models_are_permanent() {
    let r = rig(ScriptedModel::new(vec![Err(ModelError::Failed {
        code: "rate_limited",
        transient: true,
    })]));
    assert!(matches!(
        r.worker.execute(agent_run("r1")).await,
        Outcome::Failed(_)
    ));
    assert_eq!(
        lock(&r.runs.state).failed,
        [Failure::Transient("rate_limited")]
    );

    let r = rig_models(Arc::new(NoModels), config());
    assert!(matches!(
        r.worker.execute(agent_run("r2")).await,
        Outcome::Failed(_)
    ));
    assert_eq!(
        lock(&r.runs.state).failed,
        [Failure::Permanent("unknown_model")]
    );

    let mut ctx = agent_run("r3");
    ctx.work.revision = Revision("gone".into());
    let r = rig(ScriptedModel::new(vec![say("x")]));
    assert!(matches!(r.worker.execute(ctx).await, Outcome::Failed(_)));
    assert_eq!(
        lock(&r.runs.state).failed,
        [Failure::Transient("unknown_revision")]
    );
}

#[tokio::test]
async fn an_endless_tool_loop_stops_at_the_turn_limit() {
    let script: Vec<_> = (0..8)
        .map(|n| calls("continue", &[(&format!("r1:{n}:0"), "agenda.list", "{}")]))
        .collect();
    let r = rig(ScriptedModel::new(script));
    assert!(matches!(
        r.worker.execute(agent_run("r1")).await,
        Outcome::Failed(_)
    ));
    assert_eq!(
        lock(&r.runs.state).failed,
        [Failure::Permanent("too_many_turns")]
    );
    assert_eq!(r.broker.calls().len(), 6);
}

#[tokio::test]
async fn a_model_turn_without_valid_calls_is_rejected() {
    for bad in [
        calls("", &[]),
        calls(
            "",
            &[("same", "agenda.list", "{}"), ("same", "agenda.list", "{}")],
        ),
    ] {
        let r = rig(ScriptedModel::new(vec![bad]));
        assert!(matches!(
            r.worker.execute(agent_run("r1")).await,
            Outcome::Failed(_)
        ));
        assert_eq!(
            lock(&r.runs.state).failed,
            [Failure::Permanent("invalid_model_output")]
        );
        assert!(r.broker.calls().is_empty());
    }
}

#[tokio::test]
async fn long_answers_are_clipped_and_empty_ones_fail() {
    let r = rig(ScriptedModel::new(vec![say(&"€".repeat(13_000))]));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    let sent = lock(&r.runs.state).finished[0].answers[0].0.clone();
    let text = serde_json::from_str::<serde_json::Value>(&sent).unwrap()["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(text.chars().count(), 12_000);

    let r = rig(ScriptedModel::new(vec![say("   ")]));
    assert!(matches!(
        r.worker.execute(agent_run("r2")).await,
        Outcome::Failed(_)
    ));
    assert_eq!(
        lock(&r.runs.state).failed,
        [Failure::Permanent("empty_answer")]
    );
}

struct OneShot(&'static str);

impl ModelRunner for OneShot {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        assert!(request.exchanges.is_empty() && request.relay.is_none());
        let text = self.0.to_owned();
        Box::pin(async move {
            Ok(ModelStep::Final {
                text,
                usage: Default::default(),
                outcome: Default::default(),
            })
        })
    }
}

#[tokio::test]
async fn a_loop_runner_is_called_once_and_the_worker_does_not_drive_its_turns() {
    let r = rig(Arc::new(OneShot("CLI handled everything")));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    let state = lock(&r.runs.state);
    assert_eq!(state.finished[0].result, answer("CLI handled everything"));
    assert!(state.steps.is_empty() && r.broker.calls().is_empty());
}
