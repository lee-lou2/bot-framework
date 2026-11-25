//! 에이전트 run의 Step 흐름: 체크포인트 순서, 재개, 승인 대기, 위임, 호출 결과 중계, 답 만들기.

use super::fakes::*;
use crate::{
    model::{
        CallEnd, ModelError, ModelRequest, ModelRunner, ModelStep, PendingCall, RunnerKind,
        StepFuture,
    },
    worker::{Outcome, checkpoint::Planned},
};
use bot_core::{command::*, id::*, port::Error, state::Visibility};
use std::sync::Arc;

fn plan(run: &str, calls: &[(&str, &str, &str)]) -> Step {
    let planned = Planned {
        text: "찾아볼게요".into(),
        calls: calls
            .iter()
            .map(|(call, tool, args)| PendingCall {
                call: CallId(format!("{run}:{call}")),
                tool: ToolId((*tool).into()),
                args: JsonPayload((*args).into()),
            })
            .collect(),
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
    let model = ScriptedModel::new(vec![say("오늘은 일정이 없어요")]);
    let r = rig(model.clone());
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    let state = lock(&r.runs.state);
    assert_eq!(state.finished.len(), 1);
    assert_eq!(state.finished[0].result, answer("오늘은 일정이 없어요"));
    assert_eq!(
        state.finished[0].answers,
        vec![answer("오늘은 일정이 없어요")]
    );
    assert!(state.steps.is_empty() && state.failed.is_empty());
    let request = &model.requests()[0];
    assert_eq!(request.attempt.run.0, "r1");
    assert!(
        request.system.contains("한국어로 짧게"),
        "맥락이 조립돼 전달된다"
    );
    assert!(request.tools.iter().any(|t| t.id.0 == "agenda.list"));
}

#[tokio::test]
async fn the_models_turn_is_checkpointed_before_its_calls_run_and_the_next_turn_sees_the_results() {
    let model = ScriptedModel::new(vec![
        calls("찾아볼게요", &[("r1:0:0", "agenda.list", "{}")]),
        say("일정이 없어요"),
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
    // 호출 실행 중에 멈춘 차례: 모델을 다시 부르지 않고 같은 호출 ID로 이어 간다
    let mut ctx = agent_run("r1");
    ctx.work.steps = vec![plan("r1", &[("0:0", "agenda.list", "{}")])];
    let model = ScriptedModel::new(vec![say("끝")]);
    let r = rig(model.clone());
    assert_eq!(r.worker.execute(ctx).await, Outcome::Succeeded);
    assert_eq!(r.broker.calls()[0].call.0, "r1:0:0");
    assert_eq!(
        r.runs.events(),
        ["call:agenda.list", "checkpoint:end-0"],
        "계획을 다시 적지 않는다"
    );
    assert_eq!(model.requests().len(), 1);
    assert_eq!(model.requests()[0].exchanges.len(), 1);

    // 끝난 차례: 호출도 모델 재호출도 없이 다음 차례로 간다
    let finished = crate::model::Exchange {
        text: "찾아볼게요".into(),
        calls: vec![],
    };
    let mut ctx = agent_run("r2");
    ctx.work.steps = vec![
        plan("r2", &[("0:0", "agenda.list", "{}")]),
        Step {
            key: "end-0".into(),
            body: finished.to_payload(),
        },
    ];
    let model = ScriptedModel::new(vec![say("바로 답")]);
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
        "지울게요",
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
        "끝나지 않은 차례"
    );
}

#[tokio::test]
async fn a_held_call_also_parks() {
    let model = ScriptedModel::new(vec![calls("보낼게요", &[("r1:0:0", "agenda.list", "{}")])]);
    let r = rig(model);
    r.broker
        .script(Ok(CallResult::Held(CallId("r1:0:0".into()))));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Parked);
}

#[tokio::test]
async fn a_denied_call_is_told_to_the_model_instead_of_failing_the_run() {
    let model = ScriptedModel::new(vec![
        calls("지울게요", &[("r1:0:0", "agenda.delete", "{}")]),
        say("권한이 없어서 못 지웠어요"),
    ]);
    let r = rig(model.clone());
    r.broker.script(Err(Error::Denied));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    assert_eq!(
        model.requests()[1].exchanges[0].calls[0].end,
        CallEnd::Denied
    );
    // 알 수 없는 도구·잘못된 인자도 모델이 고칠 수 있는 결말이다
    let model = ScriptedModel::new(vec![
        calls(
            "해볼게요",
            &[("r2:0:0", "nope", "{}"), ("r2:0:1", "agenda.list", "{}")],
        ),
        say("그래요"),
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
        "해볼게요",
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
        "해볼게요",
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
        heartbeat: std::time::Duration::from_secs(60), // 하트비트가 가짜 갱신 결과를 가로채지 않게
        ..config()
    };
    let model = ScriptedModel::new(vec![
        calls("해볼게요", &[("r1:0:0", "agenda.list", "{}")]),
        say("됐어요"),
    ]);
    let r = rig_with(model, slow.clone());
    r.broker.script(Err(Error::Stale)); // 판정에 쓴 판본이 바뀌었다
    r.broker.script(done("{}"));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    assert_eq!(r.broker.calls().len(), 2, "같은 호출 ID로 다시 부른다");
    assert_eq!(r.broker.calls()[0].call, r.broker.calls()[1].call);

    let model = ScriptedModel::new(vec![calls("해볼게요", &[("r2:0:0", "agenda.list", "{}")])]);
    let r = rig_with(model, slow);
    r.broker.script(Err(Error::Stale));
    r.runs.renew_script(Err(Error::Stale)); // 리스를 잃었다
    assert_eq!(r.worker.execute(agent_run("r2")).await, Outcome::Lost);
    let state = lock(&r.runs.state);
    assert!(
        state.finished.is_empty() && state.failed.is_empty(),
        "잃은 뒤에는 아무것도 쓰지 않는다"
    );
}

fn delegate_args(agent: &str) -> String {
    serde_json::json!({"agent": agent, "instruction": "일정 조회"}).to_string()
}

#[tokio::test]
async fn delegation_narrows_the_scopes_parks_the_parent_and_hands_the_child_result_back() {
    let mut ctx = agent_run("r1");
    ctx.scopes = scopes(&["run", "calendar.read", "admin.x"]);
    let args = delegate_args("main/research");
    let model = ScriptedModel::new(vec![calls("맡길게요", &[("r1:0:0", "delegate", &args)])]);
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
        ("r1:0:0", "main/research", "일정 조회")
    );
    assert_eq!(
        sent[0].scopes,
        scopes(&["run", "calendar.read"]),
        "요청 범위 ∩ 자식·프로필 상한 (admin.x는 상한 밖)"
    );
    assert!(lock(&r.runs.state).finished.is_empty());

    // 자식이 끝나 부모가 깨어났다: 위임을 다시 하지 않고 자식 결과를 모델에게 돌려준다
    let mut resumed = ctx.clone();
    resumed.work.steps = vec![
        plan("r1", &[("0:0", "delegate", &args)]),
        Step {
            key: "r1:0:0".into(),
            body: JsonPayload(r#"{"state":"succeeded","result":{"text":"일정 없음"}}"#.into()),
        },
    ];
    let model = ScriptedModel::new(vec![say("연구 결과: 일정 없음")]);
    let r = rig(model.clone());
    // 저장소가 자식의 대상·지시·부여 범위를 대조해 읽어 준 완료 결과
    lock(&r.delegation.results).insert(
        "r1:0:0".into(),
        JsonPayload(r#"{"state":"succeeded","result":{"text":"일정 없음"}}"#.into()),
    );
    assert_eq!(r.worker.execute(resumed).await, Outcome::Succeeded);
    assert!(lock(&r.delegation.requests).is_empty());
    assert_eq!(
        model.requests()[0].exchanges[0].calls[0].end,
        CallEnd::Done(answer("일정 없음"))
    );

    // 자식이 실패·취소로 끝나도 부모 run은 이어 간다(모델이 사정을 안다)
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
        let model = ScriptedModel::new(vec![say("알겠어요")]);
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
            calls("맡길게요", &[("r1:0:0", "delegate", &args)]),
            say("안 되네요"),
        ]);
        let r = rig(model.clone());
        assert_eq!(
            r.worker.execute(agent_run("r1")).await,
            Outcome::Succeeded,
            "{agent}"
        );
        assert!(
            lock(&r.delegation.requests).is_empty(),
            "{agent}: 저장소까지 가지 않는다"
        );
        assert_eq!(
            model.requests()[1].exchanges[0].calls[0].end,
            CallEnd::Denied
        );
    }
    // 저장소가 거절해도 모델이 볼 결말이다
    let args = delegate_args("main/research");
    let model = ScriptedModel::new(vec![
        calls("맡길게요", &[("r2:0:0", "delegate", &args)]),
        say("안 되네요"),
    ]);
    let r = rig(model.clone());
    *lock(&r.delegation.error) = Some(Error::Denied);
    assert_eq!(r.worker.execute(agent_run("r2")).await, Outcome::Succeeded);
    assert_eq!(
        model.requests()[1].exchanges[0].calls[0].end,
        CallEnd::Denied
    );
}

#[tokio::test]
async fn memory_hits_reach_the_prompt_as_untrusted_reference_and_memory_trouble_does_not_fail_the_run()
 {
    let model = ScriptedModel::new(vec![say("네")]);
    let r = rig(model.clone());
    *lock(&r.memory.hits) = vec![MemoryHit {
        id: MemoryId("m1".into()),
        visibility: Visibility::Profile,
        text: "우유는 저지방만 마신다".into(),
    }];
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    // 기억 본문은 시스템 지침이 아니라 이번 질문 바로 앞 사용자 영역의 비신뢰 참고 자료다
    let request = &model.requests()[0];
    assert!(!request.system.contains("우유는 저지방만 마신다"));
    let reference = &request.conversation[request.conversation.len() - 2];
    assert!(reference.text.starts_with(crate::context::REFERENCE_HEADER));
    assert!(reference.text.contains("우유는 저지방만 마신다"));

    let model = ScriptedModel::new(vec![say("네")]);
    let r = rig(model.clone());
    *lock(&r.memory.fail) = true;
    assert_eq!(r.worker.execute(agent_run("r2")).await, Outcome::Succeeded);
}

#[tokio::test]
async fn previous_turns_of_the_conversation_reach_the_model() {
    let model = ScriptedModel::new(vec![say("네")]);
    let r = rig(model.clone());
    lock(&r.runs.state).history = vec![
        Turn {
            from: Some(PrincipalId("local:owner".into())),
            text: "어제 뭐 했어".into(),
        },
        Turn {
            from: None,
            text: "산책했어요".into(),
        },
    ];
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    let texts: Vec<_> = model.requests()[0]
        .conversation
        .iter()
        .map(|m| m.text.clone())
        .collect();
    assert_eq!(texts[..2], ["어제 뭐 했어", "산책했어요"]);
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

    // 판본을 찾지 못하면 카탈로그가 아직 준비 안 됐을 수 있어 다시 시도한다
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
        .map(|n| calls("계속", &[(&format!("r1:{n}:0"), "agenda.list", "{}")]))
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
    let r = rig(ScriptedModel::new(vec![say(&"가".repeat(13_000))]));
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

/// 자기 도구 순환을 가진 실행기(CLI 방식): 한 번 불리고 최종 답만 돌려준다.
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
    let r = rig(Arc::new(OneShot("CLI가 알아서 했어요")));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Succeeded);
    let state = lock(&r.runs.state);
    assert_eq!(state.finished[0].result, answer("CLI가 알아서 했어요"));
    assert!(state.steps.is_empty() && r.broker.calls().is_empty());
}
