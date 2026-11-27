//! Streamable-HTTP MCP relay (JSON responses only) exposing agent skill and built-in tools to the VM model CLI.
//!
//! - Tool names map `skill.tool` <-> `skill_tool` (Anthropic rejects `.`; skill/tool IDs forbid `_`).
//! - Call identity derives from `_meta["claudecode/toolUseId"]` via `CallBook` so resumes reuse the same call ID.
//! - Approval waits, holds, and refusals return only fixed `isError` vocabulary words (never nonces or internal details).
//! - Same call positions serialize via `slots` so concurrent redeliveries cannot split into both refusal and execution.

use super::{
    Session, Shared,
    delegate::{self, Delegated},
    owner::revoked,
    park::ParkReason,
    refuse,
};
use crate::{
    canon::canonicalize, json::parse, model::Protocol, service::schedule, text::json_quote,
};
use bot_core::{
    cli::{ToolOutput, error_word},
    command::{CallResult, JsonPayload, ToolCall},
    id::*,
    port::Error,
};
use serde_json::Value;
use tokio::sync::watch;

/// Supported MCP protocol versions (newest first); unknown versions fall back to `SUPPORTED[0]`.
const SUPPORTED: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Extracts a clean protocol-specific call ID (`claudecode/toolUseId` or Codex `callId`).
fn tool_use_id(protocol: Protocol, meta: Option<&Value>) -> Option<String> {
    let key = match protocol {
        Protocol::Anthropic => "claudecode/toolUseId",
        Protocol::OpenAiResponses => "callId",
        Protocol::Gemini => return None,
    };
    meta.and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .filter(|t| {
            !t.is_empty()
                && t.len() <= 64
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .map(str::to_owned)
}

/// Extracts AGY's `_meta.progressToken` (`<traj_id>:<step_idx>` or integer) for idempotent FIFO slot binding.
fn progress_token(meta: Option<&Value>) -> Option<String> {
    match meta?.get("progressToken")? {
        Value::String(s)
            if !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            Some(s.clone())
        }
        Value::Number(n) => n.as_u64().map(|v| v.to_string()),
        _ => None,
    }
}

fn rpc_result(id: &Value, result: &str) -> Vec<u8> {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{result}}}",
        serde_json::to_string(id).unwrap_or_else(|_| "null".into())
    )
    .into_bytes()
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Vec<u8> {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":{code},\"message\":{}}}}}",
        serde_json::to_string(id).unwrap_or_else(|_| "null".into()),
        json_quote(message)
    )
    .into_bytes()
}

fn notice(word: &'static str) -> String {
    serde_json::to_string(&ToolOutput::notice(word)).expect("JSON tool output")
}

fn mcp_result(payload: &str) -> String {
    serde_json::to_string(&ToolOutput::from_payload(payload)).expect("JSON tool output")
}

/// Processes one JSON-RPC request and returns `(http_status, body)`; notifications return `202` with an empty body.
pub(super) async fn handle(
    shared: &Shared,
    session: &Session,
    gone: &watch::Receiver<bool>,
    body: &[u8],
) -> (u16, Vec<u8>) {
    let Some(Value::Object(frame)) = std::str::from_utf8(body).ok().and_then(parse) else {
        return (400, rpc_error(&Value::Null, -32700, "parse error"));
    };
    let Some(method) = frame.get("method").and_then(Value::as_str) else {
        // Response frames (we never initiate client requests) are acknowledged and ignored.
        return (202, Vec::new());
    };
    let Some(id) = frame
        .get("id")
        .filter(|id| id.is_string() || id.is_number())
    else {
        return (202, Vec::new());
    };
    let params = frame.get("params");
    let reply = match method {
        "initialize" => {
            let asked = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .filter(|v| SUPPORTED.contains(v))
                .unwrap_or(SUPPORTED[0]);
            rpc_result(
                id,
                &format!(
                    "{{\"protocolVersion\":{},\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"bot-broker\",\"version\":\"0.1.0\"}}}}",
                    json_quote(asked)
                ),
            )
        }
        "ping" => rpc_result(id, "{}"),
        "tools/list" => tools_list(shared, session, id).await,
        "tools/call" => tools_call(shared, session, gone, id, params).await,
        _ => rpc_error(id, -32601, "method not found"),
    };
    (200, reply)
}

async fn tools_list(shared: &Shared, session: &Session, id: &Value) -> Vec<u8> {
    let Ok(ctx) = shared.parts.model.verify(session.attempt.clone()).await else {
        return rpc_error(id, -32001, "session is no longer current");
    };
    if ctx.agent != session.agent {
        return rpc_error(id, -32001, "session is no longer current");
    }
    let mut tools = Vec::new();
    if let Some(snapshot) = shared.parts.catalog.current(&session.attempt.profile)
        && let Some(agent) = snapshot.agents.get(&session.agent)
    {
        let effective: bot_core::profile::Scopes =
            bot_core::scope::effective(&snapshot, &ctx.principal, &session.agent)
                .map(|allowed| {
                    ctx.scopes
                        .iter()
                        .filter(|s| allowed.contains(*s))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
        let super_admin = snapshot.is_super_admin(&ctx.principal);
        for spec in bot_core::builtin::model_tools_for(&effective, super_admin) {
            let name = ToolId(spec.id.into()).mcp_name();
            tools.push((
                name.clone(),
                format!(
                    "{{\"description\":{},\"inputSchema\":{},\"name\":{}}}",
                    json_quote(spec.description),
                    spec.schema,
                    json_quote(&name)
                ),
            ));
        }
        if let Some(entry) = delegate::listing(&snapshot, &session.agent) {
            tools.push(entry);
        }
        for skill in agent.skills.values() {
            for (tool_id, tool) in &skill.tools {
                // Built-in tool names are reserved; filter by effective scopes (broker re-checks on every call).
                if bot_core::builtin::reserved(&tool_id.0)
                    || tool.scopes.is_empty()
                    || !tool.scopes.iter().all(|s| effective.contains(s))
                {
                    continue;
                }
                let Some(schema) = parse(&tool.input.0).and_then(|v| canonicalize(&v)) else {
                    continue;
                };
                let name = tool_id.mcp_name();
                let description = tool.model_description();
                tools.push((
                    name.clone(),
                    format!(
                        "{{\"description\":{},\"inputSchema\":{schema},\"name\":{}}}",
                        json_quote(description.as_deref().unwrap_or(&tool_id.0)),
                        json_quote(&name)
                    ),
                ));
            }
        }
    }
    tools.sort();
    rpc_result(
        id,
        &format!(
            "{{\"tools\":[{}]}}",
            tools
                .into_iter()
                .map(|(_, t)| t)
                .collect::<Vec<_>>()
                .join(",")
        ),
    )
}

/// Cancels `wait` and returns `None` if the session is revoked before or right as `wait` completes,
/// enforcing zero new executions after revocation.
pub(super) async fn unless_revoked<T>(
    gone: &watch::Receiver<bool>,
    wait: impl Future<Output = T>,
) -> Option<T> {
    let mut signal = gone.clone();
    tokio::select! {
        biased;
        _ = signal.changed() => None,
        output = wait => (!revoked(gone)).then_some(output),
    }
}

async fn tools_call(
    shared: &Shared,
    session: &Session,
    gone: &watch::Receiver<bool>,
    id: &Value,
    params: Option<&Value>,
) -> Vec<u8> {
    if revoked(gone) {
        return rpc_result(id, &notice("stale"));
    }
    let Some(params) = params.and_then(Value::as_object) else {
        return rpc_error(id, -32602, "invalid params");
    };
    let Some(raw_name) = params
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return rpc_result(id, &notice("invalid"));
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    if !args.is_object() {
        return rpc_result(id, &notice("invalid"));
    }
    let Some(args) = canonicalize(&args) else {
        return rpc_result(id, &notice("invalid"));
    };
    let name = raw_name.to_owned();
    // Bind AGY's FIFO slot before `ToolId` validation so hallucinated names consume their slot cleanly.
    let tool_use = match session.protocol {
        Protocol::Gemini => {
            let Some(token) = progress_token(params.get("_meta")) else {
                return rpc_result(id, &notice("invalid"));
            };
            match shared.agy.resolve(&session.attempt, &token, &name, &args) {
                Ok(resolved) => resolved,
                Err(error) => return rpc_result(id, &notice(error_word(&error))),
            }
        }
        other => {
            let Some(id_str) = tool_use_id(other, params.get("_meta")) else {
                return rpc_result(id, &notice("invalid"));
            };
            id_str
        }
    };
    let delegating = name == delegate::NAME;
    let Some(tool) = ToolId::from_mcp_name(&name)
        .or_else(|| delegating.then(|| ToolId(delegate::NAME.to_owned())))
    else {
        return rpc_result(id, &notice("invalid"));
    };
    // Gate planning, slot acquisition, and marker reads with `unless_revoked` so revocation stops execution before calling the broker.
    let book = shared.parts.calls.call_id(
        session.attempt.clone(),
        tool_use,
        name,
        JsonPayload(args.clone()),
    );
    let call = match unless_revoked(gone, book).await {
        None => return rpc_result(id, &notice("stale")),
        Some(Ok(call)) => call,
        Some(Err(error)) => return rpc_result(id, &notice(error_word(&error))),
    };
    // Serialize read-marker -> call -> write-marker per call position so concurrent redeliveries cannot both refuse and execute.
    let position = (
        session.attempt.profile.clone(),
        session.attempt.run.clone(),
        call.clone(),
    );
    let Some(_turn) = unless_revoked(gone, shared.slots.enter(position)).await else {
        return rpc_result(id, &notice("stale"));
    };
    let marker = shared
        .parts
        .calls
        .marked(session.attempt.clone(), call.clone());
    match unless_revoked(gone, marker).await {
        None => return rpc_result(id, &notice("stale")),
        Some(Ok(Some(refusal))) => return rpc_result(id, &notice(refusal.word())),
        Some(Ok(None)) => {}
        Some(Err(error)) => return rpc_result(id, &notice(error_word(&error))),
    }
    // Host-only delivery tool is hidden from models and sealed as `unknown_tool`.
    if tool.0 == schedule::DELIVER_TOOL {
        let asked = (call, tool, JsonPayload(args));
        let word = refuse::word(shared, session, asked, &Error::Missing).await;
        return rpc_result(id, &notice(if revoked(gone) { "stale" } else { word }));
    }
    if delegating {
        let outcome = delegate::call(shared, session, &call, &JsonPayload(args.clone())).await;
        let (result, parked) = match outcome {
            _ if revoked(gone) => (notice("stale"), None),
            Ok(Delegated::Result(result)) => (result, None),
            Ok(Delegated::Waiting) => (notice("child_pending"), Some(ParkReason::Child)),
            Err(error) => {
                let asked = (
                    call.clone(),
                    ToolId(delegate::NAME.to_owned()),
                    JsonPayload(args),
                );
                let word = refuse::word(shared, session, asked, &error).await;
                (notice(if revoked(gone) { "stale" } else { word }), None)
            }
        };
        park(shared, session, parked);
        return rpc_result(id, &result);
    }
    let asked = (call.clone(), tool.clone(), JsonPayload(args.clone()));
    let call = ToolCall {
        attempt: session.attempt.clone(),
        call,
        tool,
        args: JsonPayload(args),
    };
    let outcome = shared.parts.broker.call(call).await;
    let (result, parked) = match outcome {
        // Receipts for any started effect remain durable in the ledger even if the session was revoked mid-call.
        _ if revoked(gone) => (notice("stale"), None),
        Ok(CallResult::Done(payload)) => (mcp_result(&payload.0), None),
        Ok(CallResult::Awaiting(_)) => (notice("approval_pending"), Some(ParkReason::Approval)),
        Ok(CallResult::Held(_)) => (notice("held"), Some(ParkReason::Held)),
        // `bot/pass` only applies to host-initiated direct trigger calls, never model calls.
        Ok(CallResult::Passed) => (notice("unavailable"), None),
        Err(error) => {
            let word = refuse::word(shared, session, asked, &error).await;
            (notice(if revoked(gone) { "stale" } else { word }), None)
        }
    };
    park(shared, session, parked);
    rpc_result(id, &result)
}

/// Revokes this attempt's session and notifies the host once the ledger has parked the run,
/// preventing the CLI from making further model calls or finishing early.
fn park(shared: &Shared, session: &Session, reason: Option<ParkReason>) {
    if let Some(reason) = reason {
        shared.agy.clear(&session.attempt);
        shared.parts.sessions.revoke_attempt(&session.attempt);
        shared.parts.park.parked(&session.attempt, reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_round_trip_and_reject_ambiguity() {
        let id = ToolId("agenda.delete".into());
        assert_eq!(id.mcp_name(), "agenda_delete");
        assert_eq!(ToolId::from_mcp_name("agenda_delete"), Some(id));
        assert_eq!(
            ToolId::from_mcp_name("my-skill_do-it").unwrap().0,
            "my-skill.do-it"
        );
        for bad in ["agenda", "_x", "x_", "a_b_c", "", "a.b"] {
            assert!(
                ToolId::from_mcp_name(bad).is_none() || bad == "a.b",
                "{bad}"
            );
        }
        assert!(ToolId::from_mcp_name("a.b").is_none());
    }

    #[test]
    fn tool_use_ids_must_be_clean_and_present() {
        let meta = |v: &str| serde_json::json!({"claudecode/toolUseId": v, "progressToken": 2});
        assert_eq!(
            tool_use_id(Protocol::Anthropic, Some(&meta("toolu_01-AbC"))).as_deref(),
            Some("toolu_01-AbC")
        );
        for bad in ["", "has space", "evil/../x", "a;b"] {
            assert!(
                tool_use_id(Protocol::Anthropic, Some(&meta(bad))).is_none(),
                "{bad}"
            );
        }
        assert!(tool_use_id(Protocol::Anthropic, Some(&meta(&"x".repeat(65)))).is_none());
        assert!(
            tool_use_id(Protocol::Anthropic, None).is_none()
                && tool_use_id(Protocol::Anthropic, Some(&serde_json::json!({}))).is_none()
        );
        let codex = serde_json::json!({"callId": "call_spike_1", "progressToken": 1});
        assert_eq!(
            tool_use_id(Protocol::OpenAiResponses, Some(&codex)).as_deref(),
            Some("call_spike_1")
        );
        assert!(tool_use_id(Protocol::Anthropic, Some(&codex)).is_none());
        let agy = serde_json::json!({"progressToken": "traj-1:2"});
        assert_eq!(progress_token(Some(&agy)).as_deref(), Some("traj-1:2"));
        assert!(progress_token(Some(&serde_json::json!({"progressToken": "bad token"}))).is_none());
    }

    #[test]
    fn results_are_always_mcp_shaped() {
        assert_eq!(
            mcp_result(r#"{"content":[],"isError":false}"#),
            r#"{"content":[],"isError":false}"#
        );
        let wrapped = mcp_result(r#"{"receipt":"evt-9"}"#);
        assert!(wrapped.contains(r#""isError":false"#) && wrapped.contains("receipt"));
        assert_eq!(
            notice("denied"),
            r#"{"content":[{"text":"denied","type":"text"}],"isError":true}"#
        );
    }
}
