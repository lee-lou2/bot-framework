//! OpenAI 호환 입구로 접수된 실행의 일반 도구 승인 경로.
//!
//! 승인자는 접수 때 기록한 값이 아니라 **루트의 인증된 HTTP 주체와 정확한 원래 API 바인딩**으로 스냅숏에서 다시 읽는다(`Snapshot::approval_route`). 카드를
//! 열 때·누를 때·승인을 소비할 때·재개할 때마다 실행이 고정한 판본과 지금 활성 판본 **둘**에서 읽고, 사람·전달 경로·원래 모델·에이전트가 같을 때만 연다.
//! 나중에 설정을 넣거나 사람을 바꿔서 옛 요청의 승인을 얻을 수 없다. 어긋나면 열지 않고 닫힌 낱말(`ApprovalBlock`)로 이유를 낸다.
//! 이 경로를 쓸지는 접수 원장이 보증한 출처(`RunContext.api`)로 정한다. 레거시 webhook이 같은 이름을 쓰거나 본문에 비슷한 필드가 있어도 증거가 아니다:
//! 출처가 없으면 이 경로가 아니라 기존 규칙(요청자 본인)을 따른다.

use bot_core::{
    api::ApprovalBlock,
    id::{AgentId, BindingId, PrincipalId},
    profile::{Adapter, ApprovalRoute, Snapshot},
    state::RunContext,
};

/// 한 판본이 원래 요청에 대해 가리키는 사실: 입구·공개 모델·에이전트와 승인 경로.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Origin {
    pub endpoint: String,
    pub model: String,
    pub agent: AgentId,
    pub route: ApprovalRoute,
}

impl Origin {
    /// 이 판본에서 `binding`을 통해 인증된 `requester`가 가진 승인 경로. 바인딩이 OpenAI 모델이 아니거나 경로가 어긋나면 `None`이다.
    pub fn in_snapshot(
        snapshot: &Snapshot,
        binding: &BindingId,
        requester: &PrincipalId,
    ) -> Option<Self> {
        let route = snapshot.approval_route(binding, requester)?;
        let found = snapshot.bindings.get(binding)?;
        let Adapter::OpenAi {
            endpoint, model, ..
        } = &found.source
        else {
            return None;
        };
        Some(Self {
            endpoint: endpoint.clone(),
            model: model.clone(),
            agent: found.agent.clone(),
            route,
        })
    }
}

/// 고정 판본과 지금 활성 판본이 같은 경로를 가리킬 때만 연다.
/// - 고정 판본에 경로가 없다 → `NoApprover`
/// - 고정 판본에는 있었는데 지금은 없다(지정·멤버·바인딩 철회) → `ApproverRevoked`
/// - 둘 다 있는데 사람·전달 경로·바인딩의 모델·에이전트가 다르다(재지정) → `RouteChanged`
pub fn open(pinned: Option<Origin>, current: Option<Origin>) -> Result<Origin, ApprovalBlock> {
    match (pinned, current) {
        (None, _) => Err(ApprovalBlock::NoApprover),
        (Some(_), None) => Err(ApprovalBlock::ApproverRevoked),
        (Some(pinned), Some(current)) if pinned == current => Ok(pinned),
        (Some(_), Some(_)) => Err(ApprovalBlock::RouteChanged),
    }
}

/// 이 실행이 OpenAI 호환 입구로 접수됐으면(`RunContext.api`) 그 승인 경로를 정한다. 아니면 `None`이다(기존 규칙을 따른다).
/// 접수 원장이 보증한 입구·모델이 고정 판본의 바인딩과 다르면 증거가 어긋난 것이라 고정 판본에 경로가 없는 것과 같다(닫는다).
pub fn for_run(
    ctx: &RunContext,
    pinned: &Snapshot,
    current: &Snapshot,
) -> Option<Result<Origin, ApprovalBlock>> {
    let api = ctx.api.as_ref()?;
    let read = |snapshot: &Snapshot| Origin::in_snapshot(snapshot, &ctx.binding, &ctx.principal);
    let pinned = read(pinned).filter(|o| o.endpoint == api.endpoint && o.model == api.model);
    Some(open(pinned, read(current)))
}

#[cfg(test)]
mod tests;
