//! 내장 `delegate`: 직계 자식 에이전트에게 일을 맡기고, 끝난 결과를 같은 도구 사용에 돌려준다.
//!
//! 인가는 현재 판본과 실행이 고정한 판본 둘 다에서 본다: 자식은 두 판본 모두에서 이 에이전트의 직계여야 하고, 자식에게 넘기는
//! 범위는 이 run의 범위 ∩ 두 판본에서 요청자가 자식 에이전트로 쓸 수 있는 범위다. 단계 키는 논리 호출 ID 그대로라
//! 같은 단계의 재전달은 같은 자식이고(저장소가 (부모, 단계)로 유일하게 보장), 자식이 끝나 있으면 그 결과를 돌려준다.
//! 돌려주기 전에는 현재 claim·활성 판본을 저장소 기준으로 다시 확인한다.

use super::{Session, Shared};
use crate::{canon::canonicalize, json::parse, scope::delegation_scopes};
use bot_core::{
    command::{Delegate, JsonPayload},
    id::{AgentId, CallId},
    port::Error,
    profile::Snapshot,
};
use serde_json::{Value, json};

/// MCP에서 보이는 이름.
pub(super) const NAME: &str = crate::service::preflight::DELEGATE_TOOL;

/// 이 에이전트의 직계 자식(ID 순).
pub(super) fn children(snapshot: &Snapshot, agent: &AgentId) -> Vec<AgentId> {
    snapshot
        .agents
        .iter()
        .filter(|(_, a)| a.parent.as_ref() == Some(agent))
        .map(|(id, _)| id.clone())
        .collect()
}

/// `tools/list`의 도구 한 건. 직계 자식이 없으면 `None`.
pub(super) fn listing(snapshot: &Snapshot, agent: &AgentId) -> Option<(String, String)> {
    let ids: Vec<String> = children(snapshot, agent).into_iter().map(|a| a.0).collect();
    if ids.is_empty() {
        return None;
    }
    let schema = json!({
        "additionalProperties": false,
        "properties": {
            "agent": {"enum": ids, "type": "string"},
            "instruction": {"minLength": 1, "type": "string"},
        },
        "required": ["agent", "instruction"],
        "type": "object",
    });
    let schema = canonicalize(&schema)?;
    let body = format!(
        "{{\"description\":{},\"inputSchema\":{schema},\"name\":\"{NAME}\"}}",
        crate::text::json_quote("Hand a task to one of your direct sub-agents and get its result."),
    );
    Some((NAME.to_owned(), body))
}

/// 위임의 결과.
pub(super) enum Delegated {
    /// 자식이 이미 끝났다: 모델에게 돌려줄 MCP 결과.
    Result(String),
    /// 자식을 시작했고 부모가 대기에 들어갔다.
    Waiting,
}

/// 자식 결과 체크포인트(`{"state":"succeeded","result":…}` · `{"state":"dead"|"cancelled","error":코드}`)를 모델이 볼 결과로.
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
    // 현재 claim을 쥔 시도만 한다(인계됐으면 Stale).
    let ctx = shared.parts.model.verify(session.attempt.clone()).await?;
    let profile = &session.attempt.profile;
    let current = shared.parts.catalog.current(profile).ok_or(Error::Denied)?;
    let pinned = shared
        .parts
        .catalog
        .lookup(profile, &ctx.work.revision)
        .ok_or(Error::Missing)?;
    // 시작과 재개, 저장된 자식 결과를 돌려줄 때 모두 같은 인가를 먼저 통과해야 한다(요청자의 `run`·멤버 자격 포함).
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
