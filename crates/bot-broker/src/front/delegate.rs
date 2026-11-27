//! Built-in `delegate` MCP tool handler for spawning direct child agents and returning their results.
//!
//! Authorization requires the target to be a direct child in both the current and pinned snapshots,
//! narrowing child scopes to the intersection across the run and both snapshots. Re-checks the live
//! claim and active revision before starting a child or returning a checkpointed child result.

use super::{Session, Shared};
use crate::{canon::canonicalize, json::parse};
use bot_core::{
    command::{Delegate, JsonPayload},
    id::{AgentId, CallId},
    port::Error,
    profile::Snapshot,
    scope::delegation_scopes,
};
use serde_json::Value;

pub(super) const NAME: &str = crate::service::preflight::DELEGATE_TOOL;

pub(super) fn children(snapshot: &Snapshot, agent: &AgentId) -> Vec<AgentId> {
    snapshot
        .agents
        .iter()
        .filter(|(_, a)| a.parent.as_ref() == Some(agent))
        .map(|(id, _)| id.clone())
        .collect()
}

pub(super) fn listing(snapshot: &Snapshot, agent: &AgentId) -> Option<(String, String)> {
    let ids = children(snapshot, agent);
    if ids.is_empty() {
        return None;
    }
    let schema = canonicalize(&bot_core::builtin::delegate_schema(&ids))?;
    let body = format!(
        "{{\"description\":{},\"inputSchema\":{schema},\"name\":\"{NAME}\"}}",
        crate::text::json_quote(bot_core::builtin::DELEGATE_DESCRIPTION),
    );
    Some((NAME.to_owned(), body))
}

pub(super) enum Delegated {
    Result(String),
    Waiting,
}

fn child_result(body: &JsonPayload) -> String {
    use bot_core::cli::ToolOutput;
    let value = parse(&body.0).unwrap_or(Value::Null);
    let output = match value.get("state").and_then(Value::as_str) {
        Some("succeeded") => ToolOutput::child_result(&value["result"]),
        Some("cancelled") => ToolOutput::notice("child_cancelled"),
        _ => ToolOutput::notice("child_failed"),
    };
    serde_json::to_string(&output).expect("JSON tool output")
}

pub(super) async fn call(
    shared: &Shared,
    session: &Session,
    call: &CallId,
    args: &JsonPayload,
) -> Result<Delegated, Error> {
    let Some((child, instruction)) = Delegate::parse_target(args) else {
        return Err(Error::Invalid);
    };
    let ctx = shared.parts.model.verify(session.attempt.clone()).await?;
    let profile = &session.attempt.profile;
    let current = shared.parts.catalog.current(profile).ok_or(Error::Denied)?;
    let pinned = shared
        .parts
        .catalog
        .lookup(profile, &ctx.work.revision)
        .ok_or(Error::Missing)?;
    // Authorize before both starting a child and returning a checkpointed result.
    let scopes = delegation_scopes(&ctx, &current, &pinned, &child).map_err(|_| Error::Denied)?;
    let command = Delegate {
        parent: session.attempt.clone(),
        checked_revision: current.revision.clone(),
        step: call.0.clone(),
        child,
        instruction,
        scopes,
    };
    if ctx.work.steps.iter().any(|s| s.key == call.0) {
        let body = shared
            .parts
            .delegation
            .result(command)
            .await?
            .ok_or(Error::Missing)?;
        return Ok(Delegated::Result(child_result(&body)));
    }
    shared.parts.delegation.delegate(command).await?;
    Ok(Delegated::Waiting)
}
