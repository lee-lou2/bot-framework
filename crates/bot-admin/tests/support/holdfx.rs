//! 보류 시험의 공용 재료: 프로필·run·호출을 정해 보류 중인 run과 보류·단계를 쌓는다.
//! 호출·단계는 프로필이 키의 일부이므로(`OwnedCall`·`OwnedEffect`) 같은 run·호출 ID를 다른 프로필이 쓰는 경우도 만들 수 있다.
#![allow(dead_code)]

use bot_admin::{
    fake::{Data, OwnedEffect, call_view, effect_view, run_view},
    port::{EffectView, HoldKind, HoldView, Waiting},
};
use bot_core::{
    admin::CallState,
    id::{CallId, HoldId, ProfileId, RunId, ToolId},
    state::{EffectState, Risk, RunState},
};
use std::time::SystemTime;

/// 보류 하나(`id`가 None이면 단계가 있는 묶음 보류).
pub fn hold(
    kind: HoldKind,
    profile: &str,
    run: &str,
    call: &str,
    id: Option<&str>,
    steps: Vec<EffectView>,
) -> HoldView {
    HoldView {
        kind,
        hold_id: id.map(|v| HoldId(v.into())),
        profile: ProfileId(profile.into()),
        run: RunId(run.into()),
        call: CallId(call.into()),
        tool: ToolId(
            if kind == HoldKind::Reply {
                "reply"
            } else {
                "report.publish"
            }
            .into(),
        ),
        reason: "unknown_outcome".into(),
        since: SystemTime::now(),
        steps,
    }
}

/// 보류를 기다리는 run과 그 호출(`Fake::demo()`의 보류는 그대로 남는다).
pub fn waiting_run(d: &mut Data, profile: &str, run: &str, call: &str) {
    let mut r = run_view(profile, run, "main", "local:owner", RunState::Waiting);
    r.waiting = Some(Waiting::Hold(CallId(call.into())));
    d.calls.push(call_view(
        &r,
        call,
        "report.publish",
        Risk::Write,
        CallState::Held,
    ));
    d.runs.push(r);
}

/// 그 프로필·run의 호출에 달린 단계 하나(보류 식별자는 `h-<효과>`가 기본이니 같은 효과 ID를 다른 프로필이 쓰면 바꿔 준다).
pub fn step_of(
    d: &Data,
    profile: &str,
    run: &str,
    call: &str,
    effect: &str,
    state: EffectState,
) -> OwnedEffect {
    let found = d
        .calls
        .iter()
        .find(|c| c.profile.0 == profile && c.run.0 == run && c.call.0 == call)
        .expect("호출");
    effect_view(found, effect, "notify", state, None)
}

/// 단계 둘의 보류 식별자를 프로필마다 다르게 붙인다(실제 저장소의 보류 식별자는 전역에서 유일하다).
pub fn with_hold_id(mut step: OwnedEffect, id: &str) -> OwnedEffect {
    step.view.hold_id = Some(HoldId(id.into()));
    step
}

pub fn views(steps: &[OwnedEffect]) -> Vec<EffectView> {
    steps.iter().map(|e| e.view.clone()).collect()
}
