//! Pinned and current revisions must agree on approver, route, model, and agent.

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

/// Snapshot with model `assistant` (agent `main`), HTTP principal, two humans, and Slack bindings `team`/`other`.
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

/// Run originating from an authenticated OpenAI ingress (inherited by delegated children).
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
    let pinned = world("r1", None);
    let current = world("r2", Some(slack("slack:home/U123", "team")));
    assert_eq!(
        verdict(&pinned, &current),
        Some(Err(ApprovalBlock::NoApprover))
    );
    assert_eq!(
        verdict(&world("r1", None), &world("r2", None)),
        Some(Err(ApprovalBlock::NoApprover))
    );
}

#[test]
fn a_designation_a_member_or_the_binding_withdrawn_after_the_pin_is_revoked() {
    let route = slack("slack:home/U123", "team");
    let pinned = world("r1", Some(route.clone()));
    let unmapped = world("r2", None);
    let departed = world_of("r2", Some(route.clone()), "assistant", "main", false);
    let gone = snapshot("personal", "r2")
        .role("owner", &["a"])
        .member(REQUESTER, &["owner"])
        .policy_scopes(&["a"])
        .build();
    for (why, current) in [
        ("mapping removed", unmapped),
        ("member departed", departed),
        ("binding removed", gone),
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
        ("different person", slack("slack:home/U999", "team")),
        (
            "different delivery binding",
            slack("slack:home/U123", "other"),
        ),
        ("switched to local", local("local:owner")),
    ] {
        assert_eq!(
            verdict(&pinned, &world("r2", Some(route))),
            Some(Err(ApprovalBlock::RouteChanged)),
            "{why}"
        );
    }
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
            "different public model name",
            world_of("r2", Some(route.clone()), "assistant-2", "main", true),
        ),
        (
            "different agent",
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
        ("different model", "openai", "other-model"),
        ("different endpoint", "elsewhere", "assistant"),
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
    // Without an authenticated OpenAI origin in the ledger, fall back to the default rule.
    let route = slack("slack:home/U123", "team");
    let (pinned, current) = (world("r1", Some(route.clone())), world("r2", Some(route)));
    let mut ctx = ctx();
    ctx.api = None;
    assert!(for_run(&ctx, &pinned, &current).is_none());
    // Under the default rule, HTTP principals are non-human and cannot approve.
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
