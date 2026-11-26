//! 운영자가 상세에서 본 실행에 재개·취소 명령을 보낸다.

use crate::{
    api::{self, INVALID, R, S, V},
    json,
    port::{RunAction, RunCommand, RunCommandOutcome},
    resp::{Fail, ok},
};
use axum::{
    Extension,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
};
use bot_core::id::{ClaimEpoch, Generation, RunId};
use serde_json::Value;

pub(crate) async fn resume(state: S, viewer: V, target: Path<(String, String)>, bytes: Bytes) -> R {
    apply(state, viewer, target, bytes, RunAction::Resume).await
}

pub(crate) async fn cancel(state: S, viewer: V, target: Path<(String, String)>, bytes: Bytes) -> R {
    apply(state, viewer, target, bytes, RunAction::Cancel).await
}

async fn apply(
    State(st): S,
    Extension(viewer): V,
    Path((profile, run)): Path<(String, String)>,
    bytes: Bytes,
    action: RunAction,
) -> R {
    api::operator(&viewer)?;
    let data = api::body(
        &bytes,
        &["command_id", "state", "generation", "claim_epoch"],
    )?;
    let text = |key| data.get(key).and_then(Value::as_str).ok_or(INVALID);
    let number = |key| data.get(key).and_then(Value::as_u64).ok_or(INVALID);
    let command = RunCommand {
        profile: api::profile_id(&profile).ok_or(INVALID)?,
        run: RunId(api::identifier(&run)?),
        id: api::identifier(text("command_id")?)?,
        action,
        state: json::parse_run_state(text("state")?).ok_or(INVALID)?,
        generation: Generation(number("generation")?),
        claim_epoch: ClaimEpoch(number("claim_epoch")?),
    };
    match st.act.control_run(viewer.principal, command).await? {
        RunCommandOutcome::Applied => Ok(ok(&serde_json::json!({"ok": true}), None)),
        RunCommandOutcome::Held => Err(Fail(StatusCode::CONFLICT, "held")),
    }
}
