//! Step 실행(차례마다 모델을 부르는 실행기)의 사용량 합계와 공급자 상태. 합계는 모든 모델 요청이 실제로 보고됐을 때만이다: 앞 차례는 체크포인트의 기록으로, 마지막은 이번 응답으로 센다.
//! 하나라도 빠지거나(미보고·기록 없는 옛 형식) 다시 집은 run이거나 위임한 자식이 있으면 합계가 아니다 — 마지막 차례만 합계로 내지 않는다.

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
        text: "답".into(),
        usage: Usage::default(),
        outcome: RunOutcome {
            truncated: false,
            usage: usage.map(|(prompt, completion)| TokenUsage { prompt, completion }),
        },
    })
}

/// 도구 하나를 부르는 차례: 그 응답의 보고(`measured`)와 공급자 상태를 정한다.
fn turn(
    call: &str,
    measured: Option<(u64, u64)>,
    state: Option<&str>,
) -> Result<ModelStep, ModelError> {
    let Ok(ModelStep::Calls {
        text, calls, usage, ..
    }) = calls("조회할게요", &[(call, "agenda.list", "{}")])
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

/// 모델 대본대로 run 하나를 돌리고 저장소에 보고된 계량들을 돌려준다.
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
    // 두 번 도구를 부른 세 요청도 합친다
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
    // 첫 차례만 보고하지 않았다: 마지막 응답의 값만 합계로 내지 않는다
    let first = run(
        vec![turn("r1:0:0", None, None), final_reply(Some((9, 4)))],
        agent_run("r1"),
    )
    .await;
    assert_eq!(first, vec![RunOutcome::default()]);
    // 마지막 응답만 보고하지 않았다
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

/// 첫 차례가 체크포인트에 끝난 run(계획의 사용량 기록은 `usage`): 이번 시도는 마지막 응답만 만든다. `delegation`은 그 첫 호출이 위임이었는가와 자식이 실제로 만들어졌는가다
/// (자식을 만들면 저장소가 위임 호출의 호출 ID로 단계를 적는다).
fn resumed(run: &str, usage: TurnUsage, delegation: Delegation) -> RunContext {
    let call = CallId(format!("{run}:0:0"));
    let delegate = delegation != Delegation::None;
    let tool = ToolId(if delegate { "delegate" } else { "agenda.list" }.into());
    let planned = Planned {
        text: "조회할게요".into(),
        calls: vec![PendingCall {
            call: call.clone(),
            tool: tool.clone(),
            args: JsonPayload("{}".into()),
        }],
        usage,
        state: None,
    };
    let finished = Exchange {
        text: "조회할게요".into(),
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

/// 첫 호출이 위임이었는가와 자식이 만들어졌는가
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Delegation {
    /// 위임이 아닌 도구 호출
    None,
    /// 위임이 실패했고 자식을 만들지 못했다(대상 오류·거절)
    NoChild,
    /// 위임했고 자식이 만들어졌다
    Child,
}

#[tokio::test]
async fn a_resumed_run_counts_the_checkpointed_turns_and_only_a_recorded_report_is_a_basis() {
    let reported = TurnUsage::Reported(TokenUsage {
        prompt: 5,
        completion: 2,
    });
    // 승인 대기 뒤 재개처럼 같은 세대로 이어 간 run: 앞 차례의 기록 + 마지막 응답
    let outcomes = run(
        vec![final_reply(Some((9, 4)))],
        resumed("r1", reported, Delegation::None),
    )
    .await;
    assert_eq!(outcomes, vec![total(14, 6)]);
    // 보고하지 않은 차례·기록이 없는 옛 형식의 차례는 근거가 아니다
    for usage in [TurnUsage::Unreported, TurnUsage::Unknown] {
        let outcomes = run(
            vec![final_reply(Some((9, 4)))],
            resumed("r2", usage, Delegation::None),
        )
        .await;
        assert_eq!(outcomes, vec![RunOutcome::default()], "{usage:?}");
    }
    // 실제로 만든 자식의 요청은 이 합에 없다
    let outcomes = run(
        vec![final_reply(Some((9, 4)))],
        resumed("r3", reported, Delegation::Child),
    )
    .await;
    assert_eq!(outcomes, vec![RunOutcome::default()]);
    // 자식을 만들지 못한 위임은 합을 모르게 만들지 않는다(그 호출의 모델 요청은 이 run의 것이다)
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
    // 계획(`turn-0`)과 끝난 교환(`end-0`) 모두에 같은 상태가 있고, 계획에는 그 응답의 사용량 기록도 있다
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
    // 다음 모델 요청의 같은 차례에 그대로 돌아온다
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
    assert!(state.steps.is_empty(), "계획을 적지 않았다");
    assert!(r.broker.calls().is_empty(), "도구를 부르지 않았다");
}
