//! Semantic validation of OpenAI-compatible model bindings (callers, `run` scope, and approval routes).
use crate::{
    issue::Issues,
    model::{Agent, Binding, ProfileParts},
};
use bot_core::{
    id::{AgentId, BindingId, PrincipalId},
    profile::{Adapter, ApprovalRoute},
};
use std::collections::BTreeMap;

/// Checks byte-only OpenAI binding invariants (host endpoint and key-slot checks run in `activate`).
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
            format!(
                "binding {}: no member matching `http:{endpoint}/...` exists, so no key can call this model",
                binding.id.0
            ),
        );
    } else if let Some(agent) = agents.get(&binding.agent)
        && (!agent.ceiling.contains("run") || !callers.iter().any(|m| m.scopes.contains("run")))
    {
        // Selecting a model dispatches to the agent immediately, so callers need `run` within roles ∩ policy ∩ ceiling.
        issues.warning(
            "openai-no-runner",
            "profile.yaml",
            format!(
                "binding {}: no member for endpoint `{endpoint}` has scope `run` on agent `{}` (check member roles, policy, and agent ceiling)",
                binding.id.0, binding.agent.0
            ),
        );
    }
    for (requester, route) in approvers {
        let at = format!("binding {} approver {}", binding.id.0, requester.0);
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
                "approver mapping key `{}` must be an HTTP principal for endpoint `{endpoint}` (`http:{endpoint}/<subject>`)",
                requester.0
            ),
        );
    } else if !p.members.contains_key(requester) {
        fail(
            "approver-requester-unknown",
            format!(
                "approver mapping requester `{}` is not listed in profile.yaml members",
                requester.0
            ),
        );
    }
    // Schema already enforces human identity kinds and `binding` presence per route variant.
    let person = route.person();
    if !p.members.contains_key(person) {
        fail(
            "approver-unknown",
            format!("approver `{}` is not listed in members", person.0),
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
                "approval delivery binding `{}` does not exist in profile.yaml (for binding {})",
                delivery.0, binding.0
            ),
        ),
        Some(Adapter::SlackChannel { workspace: w, .. }) if Some(w.as_str()) == workspace => {}
        Some(Adapter::SlackChannel { workspace: w, .. }) => fail(
            "approver-workspace-mismatch",
            format!(
                "binding `{}` workspace `{w}` does not match approver `{}` workspace; must use a channel binding in the same installation",
                delivery.0, person.0
            ),
        ),
        Some(_) => fail(
            "approver-binding-kind",
            format!(
                "binding `{}` is not a Slack channel binding; approval cards are sent via DM from the installation of a matching Slack channel binding",
                delivery.0
            ),
        ),
    }
}
