//! OpenAI 모델 바인딩의 의미 검사: 쓸 수 있는 HTTP 멤버가 있는지, 승인 매핑이 이 프로필의 멤버와 같은 설치의 Slack 채널 바인딩을 가리키는지.
//! 바이트만으로 정해지는 검사만 한다(호스트 입구·키 슬롯 대조는 activate가 맡는다).
use crate::{
    issue::Issues,
    model::{Agent, Binding, ProfileParts},
};
use bot_core::{
    id::{AgentId, BindingId, PrincipalId},
    profile::{Adapter, ApprovalRoute},
};
use std::collections::BTreeMap;

/// OpenAI 모델 바인딩: 쓸 수 있는 HTTP 멤버가 있는지, 승인 매핑이 이 프로필의 멤버와 같은 설치의 Slack 채널 바인딩을 가리키는지 본다.
/// 키 슬롯의 주체는 멤버가 아니어도 되지만(요청 때 403), 승인 매핑을 적었다면 요청자와 승인할 사람 모두 유효한 멤버여야 한다.
pub fn check(
    p: &ProfileParts,
    agents: &BTreeMap<AgentId, Agent>,
    binding: &Binding,
    issues: &mut Issues,
) {
    let Adapter::OpenAi {
        endpoint,
        approvers,
        ..
    } = &binding.kind
    else {
        return;
    };
    let prefix = format!("http:{endpoint}/");
    let callers: Vec<_> = p
        .members
        .values()
        .filter(|m| m.identity.0.starts_with(&prefix))
        .collect();
    if callers.is_empty() {
        issues.warning(
            "binding-no-member",
            "profile.yaml",
            format!("바인딩 {}: 입구 `{endpoint}`의 주체에 해당하는 멤버 `http:{endpoint}/…`가 없어 어떤 키로도 이 모델을 쓸 수 없어요", binding.id.0),
        );
    } else if let Some(agent) = agents.get(&binding.agent)
        && (!agent.ceiling.contains("run") || !callers.iter().any(|m| m.scopes.contains("run")))
    {
        // 모델을 고르면 에이전트가 바로 접수하므로 호출자는 그 에이전트에 `run`(멤버 역할 ∩ 정책 ∩ 에이전트 상한)이 있어야 한다
        issues.warning(
            "openai-no-runner",
            "profile.yaml",
            format!("바인딩 {}: 입구 `{endpoint}`의 멤버 중 에이전트 `{}`에 스코프 `run`을 쓸 수 있는 멤버가 없어 이 모델을 아무도 부를 수 없어요(멤버 역할·policy·에이전트 상한을 확인하세요)", binding.id.0, binding.agent.0),
        );
    }
    for (requester, route) in approvers {
        let at = format!("바인딩 {} 승인자 {}", binding.id.0, requester.0);
        approver(p, (&binding.id, endpoint), (requester, route), &at, issues);
    }
}

fn approver(
    p: &ProfileParts,
    (binding, endpoint): (&BindingId, &str),
    (requester, route): (&PrincipalId, &ApprovalRoute),
    at: &str,
    issues: &mut Issues,
) {
    let mut fail = |code: &'static str, message: String| {
        issues.error_at(code, "profile.yaml", at, message);
    };
    if requester
        .0
        .strip_prefix("http:")
        .and_then(|r| r.split_once('/'))
        .map(|(alias, _)| alias)
        != Some(endpoint)
    {
        fail(
            "approver-requester-endpoint",
            format!(
                "승인 매핑의 키 `{}`는 이 바인딩의 입구 `{endpoint}`의 HTTP 주체(`http:{endpoint}/<주체>`)여야 해요",
                requester.0
            ),
        );
    } else if !p.members.contains_key(requester) {
        fail(
            "approver-requester-unknown",
            format!(
                "승인 매핑의 HTTP 주체 `{}`가 profile.yaml의 members에 없어요",
                requester.0
            ),
        );
    }
    // 승인할 사람이 사람 신원(`local:*`·`slack:*`)이고 경로 종류와 맞는지는 스키마가 가린다(서비스·HTTP 신원, binding 유무가 어긋나면 스키마 오류).
    let person = route.person();
    if !p.members.contains_key(person) {
        fail(
            "approver-unknown",
            format!("승인자 `{}`가 members에 없어요", person.0),
        );
    }
    let ApprovalRoute::Slack {
        binding: delivery, ..
    } = route
    else {
        return;
    };
    let workspace = person
        .0
        .strip_prefix("slack:")
        .and_then(|rest| rest.split_once('/'))
        .map(|(workspace, _)| workspace);
    match p.bindings.get(delivery).map(|b| &b.kind) {
        None => fail(
            "approver-binding-unknown",
            format!(
                "승인 카드를 보낼 설치를 확인하는 바인딩 `{}`가 profile.yaml에 없어요(승인 대상 바인딩 {})",
                delivery.0, binding.0
            ),
        ),
        Some(Adapter::SlackChannel { workspace: w, .. }) if Some(w.as_str()) == workspace => {}
        Some(Adapter::SlackChannel { workspace: w, .. }) => fail(
            "approver-workspace-mismatch",
            format!(
                "바인딩 `{}`의 워크스페이스 `{w}`와 승인자 `{}`의 워크스페이스가 달라요. 같은 설치의 채널 바인딩이어야 해요",
                delivery.0, person.0
            ),
        ),
        Some(_) => fail(
            "approver-binding-kind",
            format!(
                "바인딩 `{}`는 Slack 채널 바인딩이 아니에요. 승인 카드는 같은 설치의 Slack 채널 바인딩으로 확인한 설치의 승인자 DM으로 보내요",
                delivery.0
            ),
        ),
    }
}
