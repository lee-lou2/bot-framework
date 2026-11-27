//! Pre-approval admin API (`/v1/admin/preapprovals`): listing and CAS-guarded pause/resume (operator-only).

use crate::{
    api::{INVALID, R, S, V, body, identifier, operator, profile_id},
    json,
    resp::ok,
};
use axum::{
    body::Bytes,
    extract::{Path, RawQuery},
};
use bot_core::{
    grant::{GrantCas, TriggerKey},
    id::AgentId,
};
use serde_json::{Value, json};

/// Nested agent IDs (e.g. `main/developer`) arrive percent-encoded as a single path segment.
fn agent_id(text: &str) -> Result<AgentId, crate::resp::Fail> {
    (!text.is_empty() && text.split('/').all(bot_core::id::is_name))
        .then(|| AgentId(text.to_owned()))
        .ok_or(INVALID)
}

pub(crate) async fn list(
    axum::extract::State(st): S,
    axum::Extension(v): V,
    RawQuery(raw): RawQuery,
) -> R {
    operator(&v)?;
    if raw.as_deref().is_some_and(|q| !q.is_empty()) {
        return Err(INVALID);
    }
    let rows = st.read.preapprovals(&v).await?;
    let body = json!({"items": rows.iter().map(json::preapproval).collect::<Vec<_>>()});
    Ok(ok(&body, st.leak.as_ref()))
}

pub(crate) async fn pause(
    state: axum::extract::State<std::sync::Arc<crate::api::St>>,
    viewer: axum::Extension<bot_core::admin::Viewer>,
    path: Path<(String, String, String)>,
    bytes: Bytes,
) -> R {
    toggle(state, viewer, path, bytes, true).await
}

pub(crate) async fn resume(
    state: axum::extract::State<std::sync::Arc<crate::api::St>>,
    viewer: axum::Extension<bot_core::admin::Viewer>,
    path: Path<(String, String, String)>,
    bytes: Bytes,
) -> R {
    toggle(state, viewer, path, bytes, false).await
}

async fn toggle(
    axum::extract::State(st): S,
    axum::Extension(v): V,
    Path((profile, agent, trigger)): Path<(String, String, String)>,
    bytes: Bytes,
    pause: bool,
) -> R {
    operator(&v)?;
    let key = TriggerKey {
        profile: profile_id(&profile).ok_or(INVALID)?,
        agent: agent_id(&agent)?,
        trigger: bot_core::id::is_name(&trigger)
            .then_some(trigger)
            .ok_or(INVALID)?,
    };
    let b = body(&bytes, &["grant_id", "epoch"])?;
    let cas = GrantCas {
        grant_id: identifier(b.get("grant_id").and_then(Value::as_str).ok_or(INVALID)?)?,
        epoch: b
            .get("epoch")
            .and_then(Value::as_i64)
            .filter(|n| *n >= 0)
            .ok_or(INVALID)?,
    };
    let out = st
        .act
        .set_pause(v.principal.clone(), key, cas, pause)
        .await?;
    Ok(ok(&json::preapproval(&out.row), st.leak.as_ref()))
}
