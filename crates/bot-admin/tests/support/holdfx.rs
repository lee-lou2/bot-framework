//! Shared fixtures for hold tests: waiting runs, holds, and profile-scoped steps.
#![allow(dead_code)]

use bot_admin::fake::{Data, OwnedEffect, call_view, effect_view, run_view};
use bot_core::{
    admin::{CallState, EffectView, HoldKind, HoldView, Waiting},
    id::{CallId, HoldId, ProfileId, RunId, ToolId},
    state::{EffectState, Risk, RunState},
};
use std::time::SystemTime;

/// Builds a hold (`id = None` for a stepped hold whose individual steps carry hold IDs).
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

/// Builds a step on `(profile, run, call)` with default hold ID `h-<effect>`.
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
        .expect("call");
    effect_view(found, effect, "notify", state, None)
}

/// Overrides the step's hold ID so cross-profile fixtures mirror globally unique store IDs.
pub fn with_hold_id(mut step: OwnedEffect, id: &str) -> OwnedEffect {
    step.view.hold_id = Some(HoldId(id.into()));
    step
}

pub fn views(steps: &[OwnedEffect]) -> Vec<EffectView> {
    steps.iter().map(|e| e.view.clone()).collect()
}
