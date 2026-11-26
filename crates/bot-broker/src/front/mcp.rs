//! MCP 중계(streamable HTTP, JSON 응답만). 모델 CLI가 보는 도구는 에이전트의 스킬이 선언한 도구뿐이고,
//! 호출은 모두 브로커(`Broker::call`)를 지난다. 인가·승인·감사는 거기서 한다. 이 모듈은 모양만 옮긴다.
//!
//! - 도구 이름: `skill.tool` → `skill_tool`(Anthropic 도구 이름에 `.`이 못 들어간다. 스킬·도구 이름에는 `_`가 없어 되돌릴 수 있다).
//! - 호출 ID: CLI가 주는 `_meta."claudecode/toolUseId"`에서 정한다. 같은 도구 사용은 재개돼도 같은 호출이다.
//! - 승인 대기·보류·거절은 `isError` 결과의 고정 단어로만 알린다(nonce·내부 사정은 싣지 않는다).
//! - 같은 호출 위치(`slots`)는 한 번에 하나씩 처리한다. 먼저 남은 거절 표식을 읽어 같은 단어로 재생하고(`refuse`), 거절하면 표식을 먼저 남긴 뒤 단어를 돌려준다.

use super::{
    Session, Shared,
    delegate::{self, Delegated},
    owner::revoked,
    park::ParkReason,
    refuse,
};
use crate::{
    canon::canonicalize,
    json::parse,
    service::{preflight::noting_limit, schedule},
    text::json_quote,
};
use bot_core::{
    cli::{ToolOutput, error_word},
    command::{CallResult, JsonPayload, ToolCall},
    id::*,
    port::Error,
};
use serde_json::Value;
use tokio::sync::watch;

/// 우리가 지원하는 MCP 판본(새것부터). 요청한 판본이 이 안에 있을 때만 그대로 협상하고, 아니면 가장 새 판본으로 답한다.
const SUPPORTED: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// CLI가 주는 `toolUseId`. 없거나 모양이 틀리면 `None`이다(재전달·재기동에도 같은 호출로 처리되려면
/// 안정된 식별자가 있어야 하므로, 없으면 호출하지 않는다). 호출 ID로 바꾸는 일은 `CallBook`이 한다.
fn tool_use_id(meta: Option<&Value>) -> Option<String> {
    meta.and_then(|m| m.get("claudecode/toolUseId"))
        .and_then(Value::as_str)
        .filter(|t| {
            !t.is_empty()
                && t.len() <= 64
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .map(str::to_owned)
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

/// 요청 하나를 처리해 (HTTP 상태, 본문)을 돌려준다. 알림은 202와 빈 본문이다.
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
        // 응답 프레임(우리는 클라이언트 요청을 보내지 않는다)은 받아 두기만 한다.
        return (202, Vec::new());
    };
    let Some(id) = frame
        .get("id")
        .filter(|id| id.is_string() || id.is_number())
    else {
        return (202, Vec::new()); // 알림
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

/// 도구 목록도 현재 claim을 쥔 세션만 본다(인계됐거나 끝난 run의 세션은 목록도 못 본다).
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
        for spec in bot_core::builtin::model_tools(&agent.scopes) {
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
        // 내장 위임 도구: 직계 자식이 있을 때만 보인다(스키마가 자식 ID를 열거한다).
        if let Some(entry) = delegate::listing(&snapshot, &session.agent) {
            tools.push(entry);
        }
        for skill in agent.skills.values() {
            for (tool_id, tool) in &skill.tools {
                // 내장 도구 이름은 예약돼 있다: 같은 이름의 선언이 있어도 목록에는 내장 하나만 나온다.
                if bot_core::builtin::reserved(&tool_id.0) {
                    continue;
                }
                let Some(schema) = parse(&tool.input.0).and_then(|v| canonicalize(&v)) else {
                    continue;
                };
                let name = tool_id.mcp_name();
                tools.push((
                    name.clone(),
                    format!(
                        "{{\"description\":{},\"inputSchema\":{schema},\"name\":{}}}",
                        json_quote(&tool_id.0),
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

/// 기다리는 동안 세션이 회수되면 기다림을 버리고 `None`이다: 그 기다림 뒤에 올 일(호출·위임·표식)은 시작하지 않는다. 기다림이 끝난 바로 뒤에도 한 번 더
/// 본다 — 끝나는 순간과 회수가 겹쳐도 «회수 뒤 새 실행 0»이다(결과만 가리고 효과는 시작되는 일이 없다).
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
    // 실행에 들어가기 전: 회수된 세션으로는 아무것도 시작하지 않는다.
    if revoked(gone) {
        return rpc_result(id, &notice("stale"));
    }
    let Some(params) = params.and_then(Value::as_object) else {
        return rpc_error(id, -32602, "invalid params");
    };
    let delegating = params.get("name").and_then(Value::as_str) == Some(delegate::NAME);
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .and_then(ToolId::from_mcp_name)
        .or_else(|| delegating.then(|| ToolId(delegate::NAME.to_owned())));
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let (Some(tool), true) = (tool, args.is_object()) else {
        return rpc_result(id, &notice("invalid"));
    };
    let Some(args) = canonicalize(&args) else {
        return rpc_result(id, &notice("invalid"));
    };
    let Some(tool_use) = tool_use_id(params.get("_meta")) else {
        return rpc_result(id, &notice("invalid")); // 안정된 호출 식별자가 없으면 실행하지 않는다
    };
    // 호출 ID는 호스트의 `CallBook`이 정한다. 못 정하면 실행하지 않는다.
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    // 기다림 셋(계획·위치 문·표식 읽기)은 모두 같은 규칙이다(`unless_revoked`): 기다리는 동안 세션이 회수되면 그 뒤의 일(실행·위임·표식)은 시작하지 않고
    // 곧바로 `stale`이다. 결과만 가리고 효과는 시작되는 일이 없도록, 브로커를 부르기 전의 마지막 확인이 이 기다림들 안에 있다.
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
    // (호출 위치를 검증하기 전의 거절 — 도구 이름·인자 모양·`toolUseId`가 틀렸거나 계획과 어긋나거나 계획을 확인하지 못한 경우 — 은 봉인할 위치가 없어 단어만 돌려준다.)
    // 같은 호출 위치의 요청은 한 번에 하나씩이다: «표식을 읽고 → 호출하고 → 거절이면 표식을 남긴다»가 한 덩어리라 동시 재전달이 같은 위치를
    // 거절과 실행으로 둘 다 끝낼 수 없다.
    let position = (
        session.attempt.profile.clone(),
        session.attempt.run.clone(),
        call.clone(),
    );
    let Some(_turn) = unless_revoked(gone, shared.slots.enter(position)).await else {
        return rpc_result(id, &notice("stale"));
    };
    // 이미 거절로 닫은 위치는 다시 판정하거나 실행하지 않고 모델이 본 그 단어를 재생한다(조건이 달라졌어도 같은 호출은 같은 결과다).
    // 표식이 있는지 읽지 못하면 실행하지 않는다. 읽는 동안 세션이 회수되면 실행도 위임도 시작하지 않는다.
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
    // 호스트 전용 ID는 모델이 부를 수 없다(존재도 알리지 않는다). 검증된 위치에서 `unknown_tool`로 닫는다.
    if tool.0 == schedule::DELIVER_TOOL {
        let asked = (call, tool, JsonPayload(args));
        let word = refuse::word(shared, session, asked, &Error::Missing, false).await;
        return rpc_result(id, &notice(if revoked(gone) { "stale" } else { word }));
    }
    if delegating {
        let outcome = delegate::call(shared, session, &call, &JsonPayload(args.clone())).await;
        let (result, parked) = match outcome {
            _ if revoked(gone) => (notice("stale"), None),
            Ok(Delegated::Result(result)) => (result, None),
            // 자식을 시작했고 부모는 대기에 들어갔다: 이 CLI는 더 진행하지 못하게 한다.
            Ok(Delegated::Waiting) => (notice("child_pending"), Some(ParkReason::Child)),
            Err(error) => {
                let asked = (
                    call.clone(),
                    ToolId(delegate::NAME.to_owned()),
                    JsonPayload(args),
                );
                let word = refuse::word(shared, session, asked, &error, false).await;
                (notice(if revoked(gone) { "stale" } else { word }), None)
            }
        };
        park(shared, session, parked);
        return rpc_result(id, &result);
    }
    // 거절됐을 때 실행 전 확정 거절인지 따로 묻는 데 쓴다(호출·도구 ID와 인자는 브로커로 넘어가기 전에 챙긴다).
    let asked = (call.clone(), tool.clone(), JsonPayload(args.clone()));
    let call = ToolCall {
        attempt: session.attempt.clone(),
        call,
        tool,
        args: JsonPayload(args),
    };
    // 이 호출만의 표지로 «한도가 거절했는가»를 결과와 함께 받는다(저장소 장애의 `Unavailable`과 구별하는 원인이다).
    let (outcome, limited) = noting_limit(shared.parts.broker.call(call)).await;
    let (result, parked) = match outcome {
        // 기다리는 동안 세션이 회수됐으면 결과를 돌려주지 않는다. 이미 시작한 효과의 영수증은 원장에 남아 있다.
        _ if revoked(gone) => (notice("stale"), None),
        Ok(CallResult::Done(payload)) => (mcp_result(&payload.0), None),
        Ok(CallResult::Awaiting(_)) => (notice("approval_pending"), Some(ParkReason::Approval)),
        Ok(CallResult::Held(_)) => (notice("held"), Some(ParkReason::Held)),
        // 넘김(`bot/pass`)은 호스트가 직접 시작한 호출에서만 일어난다 — 모델의 호출에는 오지 않는다
        Ok(CallResult::Passed) => (notice("unavailable"), None),
        Err(error) => {
            let word = refuse::word(shared, session, asked, &error, limited).await;
            // 표식을 쓰는 동안 세션이 회수됐으면 결과를 돌려주지 않는다(표식은 멱등이라 남아도 된다).
            (notice(if revoked(gone) { "stale" } else { word }), None)
        }
    };
    park(shared, session, parked);
    rpc_result(id, &result)
}

/// 승인 대기·보류·위임으로 원장이 이미 run을 대기로 보냈다: 이 시도의 세션만 거두고(형제 호출의 결과는 원장에 남아 재개 때
/// 쓰인다) 호스트에 알린다. CLI가 모델을 더 부르거나 완료로 끝나지 못한다.
fn park(shared: &Shared, session: &Session, reason: Option<ParkReason>) {
    if let Some(reason) = reason {
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
            tool_use_id(Some(&meta("toolu_01-AbC"))).as_deref(),
            Some("toolu_01-AbC")
        );
        for bad in ["", "has space", "evil/../x", "a;b"] {
            assert!(tool_use_id(Some(&meta(bad))).is_none(), "{bad}");
        }
        assert!(tool_use_id(Some(&meta(&"x".repeat(65)))).is_none());
        assert!(tool_use_id(None).is_none() && tool_use_id(Some(&serde_json::json!({}))).is_none());
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
