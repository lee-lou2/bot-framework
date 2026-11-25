//! 고정·현재 두 판본이 같은 사람·경로·모델·에이전트를 가리킬 때만 연다. 나중에 설정을 넣거나 사람을 바꿔서는 옛 요청의 승인을 얻을 수 없다.

use super::*;
use crate::testing::{run_context, snapshot};
use bot_core::{api::ApiOrigin, profile::ApprovalRoute};
use std::sync::Arc;

const REQUESTER: &str = "http:openai/build";

fn slack(person: &str, binding: &str) -> ApprovalRoute {
    ApprovalRoute::Slack {
        person: PrincipalId(person.into()),
        binding: BindingId(binding.into()),
    }
}

fn local(person: &str) -> ApprovalRoute {
    ApprovalRoute::Local {
        person: PrincipalId(person.into()),
    }
}

/// 모델 `assistant`(에이전트 `main`)와 HTTP 키 주체·사람 둘·Slack 채널 바인딩 `team`·`other`가 있는 판본. `route`가 `build`의 승인 경로다.
fn world(revision: &str, route: Option<ApprovalRoute>) -> Arc<Snapshot> {
    world_of(revision, route, "assistant", "main", true)
}

fn world_of(
    revision: &str,
    route: Option<ApprovalRoute>,
    model: &str,
    agent: &str,
    people: bool,
) -> Arc<Snapshot> {
    let mut builder = snapshot("personal", revision)
        .role("owner", &["a"])
        .member("local:owner", &["owner"])
        .member(REQUESTER, &["owner"])
        .policy_scopes(&["a"])
        .slack_channel_binding("team", "main")
        .slack_channel_binding("other", "main");
    if people {
        builder = builder
            .member("slack:home/U123", &["owner"])
            .member("slack:home/U999", &["owner"]);
    }
    let approvers: Vec<(&str, ApprovalRoute)> = route.into_iter().map(|r| (REQUESTER, r)).collect();
    builder
        .openai_binding("assistant", agent, "openai", model, &approvers)
        .build()
}

/// 루트가 인증된 OpenAI 접수로 들어온 실행(위임 자식도 같은 값을 물려받는다).
fn ctx() -> RunContext {
    let mut ctx = run_context("personal", "r1", 1, REQUESTER, "main", &["a"]);
    ctx.binding = BindingId("assistant".into());
    ctx.api = Some(ApiOrigin {
        endpoint: "openai".into(),
        model: "assistant".into(),
    });
    ctx
}

fn verdict(pinned: &Snapshot, current: &Snapshot) -> Option<Result<ApprovalRoute, ApprovalBlock>> {
    for_run(&ctx(), pinned, current).map(|r| r.map(|origin| origin.route))
}

#[test]
fn the_same_person_and_route_in_both_revisions_open() {
    for route in [slack("slack:home/U123", "team"), local("local:owner")] {
        let (pinned, current) = (
            world("r1", Some(route.clone())),
            world("r2", Some(route.clone())),
        );
        assert_eq!(verdict(&pinned, &current), Some(Ok(route)));
    }
}

#[test]
fn a_route_that_was_added_after_the_pin_never_approves_an_older_request() {
    // 접수 때(고정 판본)는 승인 경로가 없었다: 지금 매핑을 넣어도 옛 요청의 승인을 얻을 수 없다
    let pinned = world("r1", None);
    let current = world("r2", Some(slack("slack:home/U123", "team")));
    assert_eq!(
        verdict(&pinned, &current),
        Some(Err(ApprovalBlock::NoApprover))
    );
    // 둘 다 없으면 당연히 없다
    assert_eq!(
        verdict(&world("r1", None), &world("r2", None)),
        Some(Err(ApprovalBlock::NoApprover))
    );
}

#[test]
fn a_designation_a_member_or_the_binding_withdrawn_after_the_pin_is_revoked() {
    let route = slack("slack:home/U123", "team");
    let pinned = world("r1", Some(route.clone()));
    // 매핑을 지웠다
    let unmapped = world("r2", None);
    // 지정된 사람이 프로필 멤버에서 빠졌다
    let departed = world_of("r2", Some(route.clone()), "assistant", "main", false);
    // 바인딩 자체가 없어졌다
    let gone = snapshot("personal", "r2")
        .role("owner", &["a"])
        .member(REQUESTER, &["owner"])
        .policy_scopes(&["a"])
        .build();
    for (why, current) in [
        ("매핑 삭제", unmapped),
        ("멤버 탈퇴", departed),
        ("바인딩 삭제", gone),
    ] {
        assert_eq!(
            verdict(&pinned, &current),
            Some(Err(ApprovalBlock::ApproverRevoked)),
            "{why}"
        );
    }
}

#[test]
fn a_different_person_or_delivery_binding_or_kind_is_a_reassignment() {
    let pinned = world("r1", Some(slack("slack:home/U123", "team")));
    for (why, route) in [
        ("다른 사람", slack("slack:home/U999", "team")),
        ("다른 전달 바인딩", slack("slack:home/U123", "other")),
        ("로컬로 바뀜", local("local:owner")),
    ] {
        assert_eq!(
            verdict(&pinned, &world("r2", Some(route))),
            Some(Err(ApprovalBlock::RouteChanged)),
            "{why}"
        );
    }
    // 로컬 → Slack도 같다
    assert_eq!(
        verdict(
            &world("r1", Some(local("local:owner"))),
            &world("r2", Some(slack("slack:home/U123", "team")))
        ),
        Some(Err(ApprovalBlock::RouteChanged))
    );
}

#[test]
fn a_reassigned_model_or_agent_is_a_reassignment_even_with_the_same_person() {
    let route = slack("slack:home/U123", "team");
    let pinned = world("r1", Some(route.clone()));
    for (why, current) in [
        (
            "다른 공개 모델 이름",
            world_of("r2", Some(route.clone()), "assistant-2", "main", true),
        ),
        (
            "다른 에이전트",
            world_of("r2", Some(route.clone()), "assistant", "main/ops", true),
        ),
    ] {
        assert_eq!(
            verdict(&pinned, &current),
            Some(Err(ApprovalBlock::RouteChanged)),
            "{why}"
        );
    }
}

#[test]
fn an_http_principal_can_never_be_the_approver() {
    // 매핑이 다른 HTTP 키를 승인자로 지정해도 경로가 아니다
    let route = slack("http:openai/other", "team");
    assert_eq!(
        verdict(&world("r1", Some(route.clone())), &world("r2", Some(route))),
        Some(Err(ApprovalBlock::NoApprover))
    );
}

#[test]
fn evidence_that_disagrees_with_the_pinned_binding_closes() {
    let route = slack("slack:home/U123", "team");
    let (pinned, current) = (world("r1", Some(route.clone())), world("r2", Some(route)));
    for (why, endpoint, model) in [
        ("다른 모델", "openai", "other-model"),
        ("다른 입구", "elsewhere", "assistant"),
    ] {
        let mut ctx = ctx();
        ctx.api = Some(ApiOrigin {
            endpoint: endpoint.into(),
            model: model.into(),
        });
        assert_eq!(
            for_run(&ctx, &pinned, &current).map(|r| r.map(|o| o.route)),
            Some(Err(ApprovalBlock::NoApprover)),
            "{why}"
        );
    }
}

#[test]
fn a_run_without_an_authenticated_openai_origin_is_not_this_path() {
    // 접수 원장의 출처가 없으면(레거시 webhook이 같은 이름을 써도) 이 경로가 아니다: 기존 규칙을 따른다
    let route = slack("slack:home/U123", "team");
    let (pinned, current) = (world("r1", Some(route.clone())), world("r2", Some(route)));
    let mut ctx = ctx();
    ctx.api = None;
    assert!(for_run(&ctx, &pinned, &current).is_none());
    // 기존 규칙에서 HTTP 주체는 사람이 아니라 승인자가 될 수 없다
    assert!(crate::approver::approver_for(&ctx, &current).is_none());
}

#[test]
fn the_comparison_itself_is_a_closed_table() {
    let origin = |person: &str| Origin {
        endpoint: "openai".into(),
        model: "assistant".into(),
        agent: AgentId("main".into()),
        route: local(person),
    };
    assert_eq!(
        open(Some(origin("local:a")), Some(origin("local:a"))),
        Ok(origin("local:a"))
    );
    assert_eq!(
        open(None, Some(origin("local:a"))),
        Err(ApprovalBlock::NoApprover)
    );
    assert_eq!(open(None, None), Err(ApprovalBlock::NoApprover));
    assert_eq!(
        open(Some(origin("local:a")), None),
        Err(ApprovalBlock::ApproverRevoked)
    );
    assert_eq!(
        open(Some(origin("local:a")), Some(origin("local:b"))),
        Err(ApprovalBlock::RouteChanged)
    );
}
