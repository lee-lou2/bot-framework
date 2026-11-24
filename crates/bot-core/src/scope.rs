//! 인가: 요청자 권한 ∩ 에이전트 상한 ∩ 프로필 상한이 도구가 요구하는 스코프 전체를 덮어야 한다.
//!
//! 요청자 권한은 접수 때 고정한 스냅샷(`RunContext.scopes`, 위임 때 좁아짐)과 지금 판본에서 그 주체의
//! 역할이 주는 스코프의 교집합이다(역할 회수가 바로 반영된다). 프로필 정책은 호출마다 현재 값으로 본다.

use crate::{
    id::*,
    profile::{Scopes, Snapshot},
    state::RunContext,
};
use std::collections::BTreeMap;

#[derive(Debug, Eq, PartialEq)]
pub enum Denial {
    /// 도구가 스코프를 하나도 선언하지 않았다. 선언 오류이므로 누구에게도 열지 않는다.
    Undeclared,
    /// 지금 판본의 멤버가 아니다.
    Member,
    /// 지금 판본에 없는 에이전트다.
    Agent,
    /// 부족한 스코프(정렬됨).
    Missing(Vec<String>),
}

impl Denial {
    pub fn code(&self) -> &'static str {
        match self {
            Denial::Undeclared => "scope_undeclared",
            Denial::Member => "member_unknown",
            Denial::Agent => "agent_unknown",
            Denial::Missing(_) => "scope_missing",
        }
    }
}

/// 이 주체의 역할이 지금 판본에서 주는 스코프의 합집합. 멤버가 아니면 `None`.
pub fn role_scopes(snapshot: &Snapshot, principal: &PrincipalId) -> Option<Scopes> {
    let roles = snapshot.members.get(principal)?;
    Some(
        roles
            .iter()
            .filter_map(|role| snapshot.roles.get(role))
            .flatten()
            .cloned()
            .collect(),
    )
}

/// 에이전트 상한. 경로(`main/research`)의 조상 항목(자기 자신 포함)을 모두 교집합한다.
/// 자식이 부모보다 넓게 지정돼 있어도 부모 상한을 넘지 못한다. 지정된 항목이 없으면 `None`이다.
pub fn agent_ceiling(agents: &BTreeMap<AgentId, Scopes>, agent: &AgentId) -> Option<Scopes> {
    let mut ceiling: Option<Scopes> = None;
    let mut path = String::new();
    for part in agent.0.split('/') {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(part);
        if let Some(limit) = agents.get(&AgentId(path.clone())) {
            ceiling = Some(match ceiling {
                None => limit.clone(),
                Some(upper) => upper.intersection(limit).cloned().collect(),
            });
        }
    }
    ceiling
}

/// 이 주체가 이 에이전트에서 지금 판본 기준으로 쓸 수 있는 스코프(역할 ∩ 프로필 상한 ∩ 에이전트 상한).
/// 멤버가 아니거나 에이전트가 없으면 `None`. 접수 시점에 요청자 권한을 고정할 때 쓴다.
pub fn effective(snapshot: &Snapshot, principal: &PrincipalId, agent: &AgentId) -> Option<Scopes> {
    let roles = role_scopes(snapshot, principal)?;
    let own = snapshot.agents.get(agent)?;
    let ceiling = agent_ceiling(&snapshot.policy.agents, agent);
    Some(
        roles
            .into_iter()
            .filter(|scope| {
                snapshot.policy.scopes.contains(scope)
                    && own.scopes.contains(scope)
                    && ceiling.as_ref().is_none_or(|upper| upper.contains(scope))
            })
            .collect(),
    )
}

/// `required`(도구가 요구하는 스코프 전체)가 모든 상한에 들어 있는가.
/// `snapshot`은 지금 활성인 판본, `pinned`는 실행이 고정한 판본이다. 에이전트 상한(`Agent.scopes`)은 두 판본 모두 지킨다
/// (접수한 `ctx.scopes`가 고정 판본의 에이전트 상한까지 담는다는 보장이 없고, 현재 정책만 다시 계산하면 권한이 넓어질 수 있다).
pub fn authorize(
    required: &Scopes,
    ctx: &RunContext,
    snapshot: &Snapshot,
    pinned: &Snapshot,
) -> Result<(), Denial> {
    if required.is_empty() {
        return Err(Denial::Undeclared);
    }
    let roles = role_scopes(snapshot, &ctx.principal).ok_or(Denial::Member)?;
    let now = snapshot.agents.get(&ctx.agent).ok_or(Denial::Agent)?;
    let then = pinned.agents.get(&ctx.agent).ok_or(Denial::Agent)?;
    let ceiling = agent_ceiling(&snapshot.policy.agents, &ctx.agent);
    let missing: Vec<String> = required
        .iter()
        .filter(|scope| {
            !(ctx.scopes.contains(*scope)
                && roles.contains(*scope)
                && snapshot.policy.scopes.contains(*scope)
                && now.scopes.contains(*scope)
                && then.scopes.contains(*scope)
                && ceiling.as_ref().is_none_or(|upper| upper.contains(*scope)))
        })
        .cloned()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Denial::Missing(missing))
    }
}

/// 에이전트를 실행하는 데 필요한 기본 스코프. 위임으로 시작하는 자식 실행도 이 스코프가 있어야 한다.
pub const RUN_SCOPE: &str = "run";

/// 직계 자식에게 위임할 때의 인가(시작과 재개, 저장된 자식 결과를 돌려줄 때 모두 같은 함수를 쓴다). 돌려주는 값은 자식에게 넘길 범위다.
///
/// 현재 판본과 실행이 고정한 판본 **둘 다**에서: 자식이 이 에이전트의 직계로 있어야 하고, 요청자가 지금도 멤버이며 자식 에이전트에서
/// (역할 ∩ 정책 ∩ 에이전트 상한으로) `run`을 쓸 수 있어야 한다. 범위는 실행이 고정한 범위(`ctx.scopes`) ∩ 두 판본에서 자식에게
/// 허용되는 범위이고, 그 안에 `run`이 없으면 자식을 시작할 수 없다.
pub fn delegation_scopes(
    ctx: &RunContext,
    current: &Snapshot,
    pinned: &Snapshot,
    child: &AgentId,
) -> Result<Scopes, Denial> {
    let direct = |snapshot: &Snapshot| {
        snapshot
            .agents
            .get(child)
            .is_some_and(|agent| agent.parent.as_ref() == Some(&ctx.agent))
    };
    if !direct(current) || !direct(pinned) {
        return Err(Denial::Agent);
    }
    let now = effective(current, &ctx.principal, child).ok_or(Denial::Member)?;
    let then = effective(pinned, &ctx.principal, child).ok_or(Denial::Member)?;
    let scopes: Scopes = ctx
        .scopes
        .iter()
        .filter(|scope| now.contains(*scope) && then.contains(*scope))
        .cloned()
        .collect();
    if scopes.contains(RUN_SCOPE) {
        Ok(scopes)
    } else {
        Err(Denial::Missing(vec![RUN_SCOPE.to_owned()]))
    }
}
