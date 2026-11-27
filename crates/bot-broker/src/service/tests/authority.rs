//! Live authority checks: stored results, approvers, revisions, and claims are re-verified at execution time.

use super::*;
use bot_core::{
    id::ClaimEpoch,
    ledger::{ApprovalState, CallLedger, CallPhase, Resolution},
    state::{ConversationKind, TriggerRef},
};

const DELETE: &str = "{\"event\":\"walk\"}";

fn without(scope: &str) -> Vec<&str> {
    ALL.iter().copied().filter(|s| *s != scope).collect()
}

fn scheduled(w: &World, run: &str, approver: &str) {
    let mut ctx = run_context(
        "personal",
        run,
        1,
        "service:morning",
        "main",
        &["calendar.delete"],
    );
    ctx.approver = Some(PrincipalId(approver.into()));
    ctx.conversation.kind = ConversationKind::Scheduled;
    ctx.trigger = Some(TriggerRef {
        agent: AgentId("main".into()),
        id: "morning".into(),
    });
    w.ledger.add_run(ctx);
}

#[test]
fn a_stored_result_is_not_returned_after_the_role_is_revoked() {
    let w = World::new();
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
    w.publish(base("r2").role("owner", &without("calendar.read")).build());
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert_eq!(
        w.runner.jobs().len(),
        1,
        "must not re-execute after revocation"
    );
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.replay".into(), "scope_missing".into()))
    );
    assert!(
        matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)),
        "stored record must remain intact"
    );
    w.publish(base("r3").build());
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
}

#[test]
fn a_recovery_receipt_is_not_returned_after_the_permission_is_revoked() {
    let w = World::new();
    w.leave_started("r1", "c1", "agenda.delete", DELETE, 1);
    w.ledger.bump_generation("r1");
    assert!(is_held(&w.call_on(
        &w.broker,
        "r1",
        2,
        "c1",
        "agenda.delete",
        DELETE
    )));
    w.ledger.resolve(
        "r1",
        "c1",
        Resolution::Executed {
            receipt: "evt-9".into(),
        },
    );
    w.publish(
        base("r2")
            .role("owner", &without("calendar.delete"))
            .build(),
    );
    assert!(matches!(
        w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(
        matches!(
            w.ledger.phase("r1", "c1"),
            CallPhase::Held {
                resolution: Some(_),
                ..
            }
        ),
        "held resolution evidence must be preserved"
    );
    assert!(w.runner.jobs().is_empty());
    w.publish(base("r3").build());
    assert_eq!(
        done(w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE)),
        "{\"receipt\":\"evt-9\"}"
    );
}

#[test]
fn a_call_left_started_stays_held_for_evidence_when_the_permission_is_revoked() {
    let w = World::new();
    w.leave_started("r1", "c1", "agenda.delete", DELETE, 1);
    w.ledger.bump_generation("r1");
    w.publish(
        base("r2")
            .role("owner", &without("calendar.delete"))
            .build(),
    );
    assert!(matches!(
        w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(
        w.runner.reaped().is_empty(),
        "must not run recovery without authority"
    );
    assert!(matches!(
        w.ledger.phase("r1", "c1"),
        CallPhase::Started { .. }
    ));
}

#[test]
fn approval_cards_follow_the_current_approver_not_the_one_recorded_at_acceptance() {
    let w = World::new();
    scheduled(&w, "r2", "local:alice");
    let old = awaiting(w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE));
    assert_eq!(
        w.ledger.stored_approval(&old).approval.approver.0,
        "local:alice"
    );
    w.publish(
        base("r2")
            .clear_triggers("main")
            .cron_trigger("main", "morning", "service:morning", Some("local:owner"))
            .build(),
    );
    assert!(
        matches!(w.decide(&old, "local:alice", true), Err(Error::Stale)),
        "old approver's button must be stale"
    );
    assert_eq!(w.ledger.approval_state(&old), ApprovalState::Superseded);
    let new = awaiting(w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE));
    assert_eq!(
        w.ledger.stored_approval(&new).approval.approver.0,
        "local:owner",
        "new card must target the new approver"
    );
    assert!(matches!(
        w.decide(&new, "local:alice", true),
        Err(Error::Denied)
    ));
    w.decide(&new, "local:owner", true).unwrap();
    done(w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn nothing_that_needs_approval_runs_when_the_current_approver_cannot_be_confirmed() {
    let w = World::new();
    scheduled(&w, "r2", "local:alice");
    w.publish(
        base("r2")
            .clear_triggers("main")
            .cron_trigger("main", "morning", "service:morning", None)
            .build(),
    );
    assert!(matches!(
        w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    let without_alice = base("r3")
        .clear_triggers("main")
        .cron_trigger("main", "morning", "service:morning", Some("local:ghost"))
        .build();
    w.publish(without_alice);
    assert!(matches!(
        w.call_on(&w.broker, "r2", 1, "c2", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(
        w.audit
            .verdicts()
            .iter()
            .filter(|(_, v)| v == "no_approver")
            .count()
            == 2
    );
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_requester_removed_from_the_profile_cannot_be_asked_or_run_anything() {
    let w = World::new();
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.publish(
        snapshot("personal", "r2")
            .role("owner", &ALL)
            .policy_scopes(&ALL)
            .agent("main")
            .build(),
    );
    assert!(
        matches!(w.decide(&nonce, "local:owner", true), Err(Error::Stale)),
        "non-member must not approve"
    );
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_revocation_between_judgement_and_start_never_reaches_the_runner() {
    let w = World::new();
    let (catalog, lowered) = (
        w.catalog.clone(),
        base("r2").role("owner", &without("calendar.read")).build(),
    );
    w.ledger
        .set_after_begin(move || catalog.publish(lowered.clone()));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert!(
        w.runner.jobs().is_empty(),
        "revocation between begin and start must not invoke runner"
    );
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Open { .. }));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn the_agent_ceiling_of_the_pinned_revision_still_applies_when_the_current_one_is_wider() {
    let w = World::new();
    let narrow_then: Vec<&str> = without("calendar.delete");
    w.publish(base("r0").agent_scopes("main", &narrow_then).build());
    w.ledger.add_run({
        let mut ctx = owner_run("r9", 1);
        ctx.work.revision = Revision("r0".into());
        ctx
    });
    w.publish(base("r1b").build());
    assert!(matches!(
        w.call_on(&w.broker, "r9", 1, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    w.publish(base("r2").agent_scopes("main", &narrow_then).build());
    assert!(matches!(
        w.call("c2", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn an_approval_survives_a_normal_reclaim_but_a_stale_claim_cannot_operate() {
    let w = World::new();
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    // Reclaiming the lease within the same generation preserves the generation-bound approval.
    let old_claim = w.ledger.claim_epoch("r1");
    let new_claim = w.ledger.bump_claim("r1");
    assert_ne!(old_claim, new_claim);
    w.decide(&nonce, "local:owner", true).unwrap();
    done(w.call("c1", "agenda.delete", DELETE));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    let stale = |claim: Option<ClaimEpoch>| Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        generation: Generation(1),
        claim,
    };
    assert!(matches!(
        block_on(w.ledger.context(stale(Some(old_claim)))),
        Err(Error::Stale)
    ));
    assert!(matches!(
        block_on(w.ledger.context(stale(None))),
        Err(Error::Denied)
    ));
    let call = ToolCall {
        attempt: stale(Some(old_claim)),
        call: CallId("c2".into()),
        tool: ToolId("agenda.list".into()),
        args: JsonPayload("{}".into()),
    };
    assert!(matches!(
        block_on(bot_core::port::Broker::call(&w.broker, call)),
        Err(Error::Stale)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn the_approval_context_is_actor_bound_and_identity_only() {
    let w = World::new();
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    let who = |p: &str| PrincipalId(p.into());
    assert!(matches!(
        block_on(
            w.ledger
                .approval_context(nonce.clone(), who("local:mallory"))
        ),
        Err(Error::Denied)
    ));
    let ctx = block_on(w.ledger.approval_context(nonce.clone(), who("local:owner"))).unwrap();
    assert!(ctx.attempt.claim.is_none(), "must carry identity only");
    assert!(
        matches!(
            block_on(w.ledger.begin(
                ctx.attempt.clone(),
                CallId("c9".into()),
                ToolId("agenda.list".into()),
                [0; 32]
            )),
            Err(Error::Denied)
        ),
        "must not authorize worker operations"
    );
    assert_eq!(ctx.principal.0, "local:owner");
    w.ledger.bump_generation("r1");
    assert!(matches!(
        block_on(w.ledger.approval_context(nonce, who("local:owner"))),
        Err(Error::Stale)
    ));
}

#[test]
fn the_stored_approval_never_keeps_a_claim() {
    let w = World::new();
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    assert!(
        w.ledger
            .stored_approval(&nonce)
            .approval
            .attempt
            .claim
            .is_none()
    );
}

#[test]
fn a_secret_the_profile_does_not_list_is_never_resolved() {
    let w = World::new();
    let mut needy = tool(Risk::Read, &["calendar.read"]);
    needy
        .secrets
        .insert("TOKEN".into(), SecretName::parse("reader").unwrap());
    w.publish(
        base("r0")
            .tool("main", "agenda.list", needy.clone())
            .build(),
    );
    w.ledger.add_run({
        let mut ctx = owner_run("r9", 1);
        ctx.work.revision = Revision("r0".into());
        ctx
    });
    assert!(matches!(
        w.call_on(&w.broker, "r9", 1, "c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.deny".into(), "secret_not_allowed".into()))
    );
    assert!(w.runner.jobs().is_empty());
    w.publish(
        base("r0")
            .secret("reader")
            .tool("main", "agenda.list", needy)
            .build(),
    );
    done(w.call_on(&w.broker, "r9", 1, "c2", "agenda.list", "{}"));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn a_stored_result_is_withheld_when_the_revision_changes_while_it_is_read() {
    let w = World::new();
    done(w.call("c1", "agenda.list", "{}"));
    let (catalog, lowered) = (
        w.catalog.clone(),
        base("r2").role("owner", &without("calendar.read")).build(),
    );
    w.ledger
        .set_after_begin(move || catalog.publish(lowered.clone()));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
}

#[test]
fn the_store_closes_the_gap_between_the_last_recheck_and_the_start_commit() {
    let w = World::new();
    let ledger = w.ledger.clone();
    w.ledger
        .set_before_start(move || ledger.set_active_revision("personal", "r2"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Stale)
    ));
    assert!(
        w.runner.jobs().is_empty(),
        "rejected start commit must not invoke runner"
    );
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Open { .. }));
}

#[test]
fn removing_only_the_origin_triggers_approver_does_not_move_the_card_to_a_sibling_trigger() {
    let w = World::new();
    scheduled(&w, "r2", "local:alice");
    // Two triggers share the service principal; removing only `morning`'s approver must not borrow `evening`'s.
    w.publish(
        base("r2")
            .clear_triggers("main")
            .cron_trigger("main", "morning", "service:morning", None)
            .cron_trigger("main", "evening", "service:morning", Some("local:owner"))
            .build(),
    );
    assert!(matches!(
        w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.deny".into(), "no_approver".into()))
    );
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_result_is_kept_in_the_ledger_but_not_returned_when_the_revision_changes_while_it_is_written() {
    let w = World::new();
    let (catalog, lowered) = (
        w.catalog.clone(),
        base("r2").role("owner", &without("calendar.read")).build(),
    );
    w.ledger
        .set_after_finish(move || catalog.publish(lowered.clone()));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert_eq!(w.runner.jobs().len(), 1, "must not re-execute");
}

#[test]
fn a_recovery_receipt_is_kept_but_not_returned_when_the_revision_changes_while_it_is_written() {
    let w = World::new();
    w.leave_started("r1", "c1", "agenda.delete", DELETE, 1);
    w.ledger.bump_generation("r1");
    assert!(is_held(&w.call_on(
        &w.broker,
        "r1",
        2,
        "c1",
        "agenda.delete",
        DELETE
    )));
    w.ledger.resolve(
        "r1",
        "c1",
        Resolution::Executed {
            receipt: "evt-9".into(),
        },
    );
    let (catalog, lowered) = (
        w.catalog.clone(),
        base("r2")
            .role("owner", &without("calendar.delete"))
            .build(),
    );
    w.ledger
        .set_after_finish(move || catalog.publish(lowered.clone()));
    assert!(matches!(
        w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE),
        Err(Error::Conflict)
    ));
    assert!(
        matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)),
        "receipt must remain preserved in the ledger"
    );
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_profile_that_is_no_longer_active_makes_the_store_refuse_to_start() {
    let w = World::new();
    w.ledger.set_active_revision("other-profile", "x");
    let ledger = w.ledger.clone();
    w.ledger
        .set_before_start(move || ledger.set_active_revision("personal", "gone"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Stale)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_tool_can_never_receive_the_hosts_private_intake_key_even_if_the_profile_lists_it() {
    let w = World::new();
    let mut greedy = tool(Risk::Read, &["calendar.read"]);
    greedy
        .secrets
        .insert("KEY".into(), SecretName::parse("private-intake").unwrap());
    w.publish(
        base("r1")
            .secret("private-intake")
            .tool("main", "agenda.list", greedy)
            .build(),
    );
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.deny".into(), "secret_reserved".into()))
    );
    assert!(w.runner.jobs().is_empty());
}
