//! Tool approvals for runs accepted via the OpenAI-compatible HTTP endpoint.
//!
//! Resolves the designated human approver from both pinned and current snapshots via the root API binding
//! and HTTP principal; verifies Slack receipt/installation alignment before consuming any approval.

use super::*;
use crate::service::route::Route;
use bot_core::{
    api::{ApiOrigin, ApprovalBlock},
    cards::CardReceipt,
    ledger::ApprovalState,
    profile::{ApprovalRoute, ProfileCatalog},
    state::{Conversation, ConversationKind, RunOrigin},
};

const HTTP: &str = "http:openai/build";
const PERSON: &str = "slack:home/U123";
const DELETE: &str = "{\"event\":\"walk\"}";

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

fn world_at(revision: &str, route: Option<ApprovalRoute>) -> Arc<Snapshot> {
    world_with(revision, route, &[])
}

fn world_with(revision: &str, route: Option<ApprovalRoute>, extra: &[&str]) -> Arc<Snapshot> {
    let approvers: Vec<(&str, ApprovalRoute)> = route.into_iter().map(|r| (HTTP, r)).collect();
    let scopes: Vec<&str> = ALL.iter().chain(extra).copied().collect();
    base(revision)
        // Keep `policy_revision` constant across revisions so tests prove approval-route verification—not policy drift—blocks stale cards.
        .policy_revision("p-openai")
        .role("owner", &scopes)
        .policy_scopes(&scopes)
        .member(HTTP, &["owner"])
        .member(PERSON, &["owner"])
        .member("slack:home/U999", &["owner"])
        .slack_channel_binding("team", "main")
        .slack_channel_binding("other", "main")
        .openai_binding("assistant", "main", "openai", "assistant", &approvers)
        .build()
}

fn openai_run(run: &str) -> RunContext {
    let mut ctx = run_context("personal", run, 1, HTTP, "main", &ALL);
    ctx.origin = RunOrigin::Http;
    ctx.conversation = Conversation {
        kind: ConversationKind::Http,
        id: "post-1".into(),
        thread: None,
    };
    ctx.binding = BindingId("assistant".into());
    ctx.api = Some(ApiOrigin {
        endpoint: "openai".into(),
        model: "assistant".into(),
    });
    ctx.approver = None;
    ctx
}

/// Registers host route mappings for workspace alias `home` under `team`; passing `previous` simulates redeploying the alias to a new team_id.
fn host_team(w: &World, previous: Option<&str>, team: &str) {
    if let Some(old) = previous {
        w.routes.unregister(&format!("{old}/C1"));
    }
    for binding in ["team", "other"] {
        w.routes.register(
            "personal",
            binding,
            ConversationKind::Channel,
            "planning",
            &format!("{team}/C1"),
        );
    }
}

fn world(pinned: Option<ApprovalRoute>) -> World {
    let w = World::new();
    w.publish(world_at("r1", pinned));
    w.ledger.add_run(openai_run("r1"));
    host_team(&w, None, "T1");
    w
}

fn receipt(owner: &str, binding: &str) -> CardReceipt {
    CardReceipt {
        team: "T1".into(),
        dm: "D1".into(),
        ts: "1700000000.000100".into(),
        owner: PrincipalId(owner.into()),
        profile: ProfileId("personal".into()),
        binding: BindingId(binding.into()),
    }
}

fn denied(result: Result<CallResult, Error>) {
    assert!(matches!(result, Err(Error::Denied)), "{result:?}");
}

fn refused_with(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("call.deny".to_owned(), word.to_owned()))
}

#[test]
fn the_designated_slack_person_approves_an_http_principals_tool_exactly_once() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(stored.approval.requester.0, HTTP);
    assert_eq!(stored.approval.approver.0, PERSON);
    assert_eq!(
        w.ledger.approval_delivery(&nonce),
        Some(BindingId("team".into()))
    );
    assert!(
        w.runner.jobs().is_empty(),
        "must not execute before approval"
    );
    // Unposted cards cannot be decided even by the designated person.
    assert!(matches!(
        w.decide(&nonce, PERSON, true),
        Err(Error::Conflict)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    for actor in [HTTP, "slack:home/U999", "local:owner", "local:alice"] {
        assert!(
            matches!(w.decide(&nonce, actor, true), Err(Error::Denied)),
            "{actor}"
        );
    }
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    // Mismatched receipt owner or delivery binding rejects the click as stale.
    for wrong in [receipt("slack:home/U999", "team"), receipt(PERSON, "other")] {
        w.ledger.post_receipt(&nonce, wrong);
        assert!(matches!(w.decide(&nonce, PERSON, true), Err(Error::Stale)));
        assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    }
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    w.decide(&nonce, PERSON, true).unwrap();
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.runner.jobs().len(), 0, "approval alone must not execute");
    assert_eq!(done(w.call("c1", "agenda.delete", DELETE)), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(done(w.call("c1", "agenda.delete", DELETE)), "{\"ok\":true}");
    w.decide(&nonce, PERSON, true).unwrap();
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    assert_eq!(w.runner.jobs()[0].principal.0, HTTP);
}

#[test]
fn a_local_route_needs_no_receipt_and_carries_no_delivery_binding() {
    let w = world(Some(local("local:owner")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    assert_eq!(
        w.ledger.stored_approval(&nonce).approval.approver.0,
        "local:owner"
    );
    assert_eq!(w.ledger.approval_delivery(&nonce), None);
    assert!(matches!(w.decide(&nonce, HTTP, true), Err(Error::Denied)));
    w.decide(&nonce, "local:owner", true).unwrap();
    assert_eq!(done(w.call("c1", "agenda.delete", DELETE)), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(
        w.routes.asked(),
        0,
        "local admin approvals must not query Slack installations"
    );
}

#[test]
fn a_tool_that_needs_no_approval_runs_without_any_route() {
    let w = world(None);
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(w.ledger.blocked_why("r1", "c1"), None);
}

#[test]
fn without_a_route_in_the_pinned_revision_nothing_runs_and_the_reason_is_stored() {
    let w = world(None);
    w.publish(world_at("r2", Some(slack(PERSON, "team"))));
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::NoApprover)
    );
    assert!(
        w.ledger.pending_approval("r1", "c1").is_none(),
        "must not open a card"
    );
    assert!(refused_with(&w, "no_approver"));
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_designation_withdrawn_while_a_card_waits_stales_the_old_button_and_blocks_the_call() {
    let w = world(Some(local("local:owner")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.publish(world_at("r2", None));
    assert!(matches!(
        w.decide(&nonce, "local:owner", true),
        Err(Error::Stale)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::ApproverRevoked)
    );
    assert!(refused_with(&w, "approver_revoked"));
}

#[test]
fn an_approved_card_whose_route_changed_before_consumption_never_runs() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    w.decide(&nonce, PERSON, true).unwrap();
    w.publish(world_at("r2", Some(slack(PERSON, "other"))));
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
    assert_ne!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::RouteChanged)
    );
}

#[test]
fn a_reassigned_person_blocks_the_call_and_the_old_person_cannot_approve() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    w.publish(world_at("r2", Some(slack("slack:home/U999", "team"))));
    assert!(matches!(w.decide(&nonce, PERSON, true), Err(Error::Stale)));
    assert!(matches!(
        w.decide(&nonce, "slack:home/U999", true),
        Err(Error::Denied)
    ));
    denied(w.call("c1", "agenda.delete", DELETE));
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::RouteChanged)
    );
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_legacy_webhook_with_the_same_name_gets_no_openai_approval_path() {
    let w = world(Some(slack(PERSON, "team")));
    let mut legacy = openai_run("r1");
    legacy.api = None;
    w.ledger.add_run(legacy);
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
    assert!(w.ledger.pending_approval("r1", "c1").is_none());
    assert_eq!(w.ledger.blocked_why("r1", "c1"), None);
    assert!(refused_with(&w, "no_approver"));
}

#[test]
fn an_owner_only_function_stays_closed_to_an_openai_principal() {
    let w = World::new();
    // Grant `memory.share` scope so the test proves ownership—not missing scope—blocks the call.
    w.publish(world_with(
        "r1",
        Some(local("local:owner")),
        &["memory.share"],
    ));
    w.ledger.add_run(openai_run("r1"));
    w.memory.add(
        "m1",
        "local:owner",
        bot_core::state::Visibility::Private,
        "secret",
    );
    denied(w.call("s1", "memory.share", "{\"id\":\"m1\"}"));
    assert!(w.memory.shared().is_empty());
    assert!(
        !w.ledger.has_call("r1", "s1"),
        "must not record call or card"
    );
    assert_eq!(w.ledger.blocked_why("r1", "s1"), None);
}

#[test]
fn a_delegated_child_uses_the_roots_original_binding_and_origin() {
    let w = world(Some(slack(PERSON, "team")));
    let mut child = openai_run("r2");
    child.root = RunId("r1".into());
    child.agent = AgentId("main".into());
    w.ledger.add_run(child);
    let nonce = awaiting(w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE));
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(stored.approval.approver.0, PERSON);
    assert_eq!(
        w.ledger.approval_delivery(&nonce),
        Some(BindingId("team".into()))
    );
    w.publish(world_at("r2", None));
    assert!(matches!(
        w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert_eq!(
        w.ledger.blocked_why("r2", "c1"),
        Some(ApprovalBlock::ApproverRevoked)
    );
    assert!(w.runner.jobs().is_empty());
}

fn approved_in_t1(w: &World) -> ApprovalId {
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    w.decide(&nonce, PERSON, true).unwrap();
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    nonce
}

#[test]
fn a_redeploy_that_only_moves_the_alias_to_another_team_never_consumes_an_approved_card() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = approved_in_t1(&w);
    // Redeploy moves alias `home` from T1 to T2 while profile revision, person, and binding stay unchanged.
    host_team(&w, Some("T1"), "T2");
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty(), "must not execute");
    assert_eq!(
        w.ledger.approval_state(&nonce),
        ApprovalState::Approved,
        "must not consume approval"
    );
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::RouteChanged)
    );
    assert!(refused_with(&w, "route_changed"));
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_card_posted_to_the_previous_team_cannot_be_approved_after_the_alias_moved() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    host_team(&w, Some("T1"), "T2");
    assert!(matches!(w.decide(&nonce, PERSON, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    let again = awaiting(w.call("c1", "agenda.delete", DELETE));
    assert_ne!(again, nonce);
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn an_unreachable_host_registry_confirms_no_block_and_the_approval_waits() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    w.routes.outage(true);
    assert!(matches!(
        w.decide(&nonce, PERSON, true),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    w.routes.outage(false);
    w.decide(&nonce, PERSON, true).unwrap();
    w.routes.outage(true);
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Unavailable)
    ));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        None,
        "transient outage must not record a block"
    );
    w.routes.outage(false);
    assert_eq!(done(w.call("c1", "agenda.delete", DELETE)), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn a_broker_without_a_host_registry_never_consumes_a_slack_card() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = approved_in_t1(&w);
    let bare = w.bare();
    assert!(matches!(
        w.call_on(&bare, "r1", 1, "c1", "agenda.delete", DELETE),
        Err(Error::Unavailable)
    ));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.ledger.blocked_why("r1", "c1"), None);
}

#[test]
fn an_approved_slack_card_without_a_confirmed_receipt_is_never_consumed() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = approved_in_t1(&w);
    w.ledger.clear_receipt(&nonce);
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::RouteChanged)
    );
}

#[test]
fn the_current_site_is_read_through_the_registry_alias_of_the_delivery_binding() {
    let w = world(Some(slack(PERSON, "team")));
    let profile = ProfileId("personal".into());
    let snapshot = w.catalog.current(&profile).unwrap();
    let site = |binding: &str, posted: &str| {
        w.broker
            .site_state(&profile, &snapshot, &BindingId(binding.into()), posted)
    };
    assert_eq!(site("team", "T1"), Route::Same);
    assert_eq!(site("team", "T2"), Route::Gone, "different team");
    assert_eq!(
        site("assistant", "T1"),
        Route::Gone,
        "not a Slack channel binding"
    );
    assert_eq!(site("missing", "T1"), Route::Gone, "unknown binding");
    w.routes.unregister("T1/C1");
    assert_eq!(site("team", "T1"), Route::Gone, "alias unregistered");
    // Non-canonical value without `team/channel` cannot identify a team.
    w.routes.register(
        "personal",
        "team",
        ConversationKind::Channel,
        "planning",
        "T1",
    );
    assert_eq!(site("team", "T1"), Route::Gone);
    w.routes.outage(true);
    assert_eq!(site("team", "T1"), Route::Unsure);
    assert_eq!(
        w.bare()
            .site_state(&profile, &snapshot, &BindingId("team".into()), "T1"),
        Route::Unsure
    );
}
