use super::*;
use bot_core::{
    cards::{LABEL_CONTENT, LABEL_RECIPIENT},
    ledger::{ApprovalState, CallPhase},
};

const DELETE: &str = "{\"event\":\"walk\"}";

fn first_approval(w: &World, call: &str, tool: &str, args: &str) -> ApprovalId {
    awaiting(w.call(call, tool, args))
}

#[test]
fn irreversible_waits_for_approval_then_runs_exactly_once() {
    let w = World::new();
    let nonce = first_approval(&w, "c1", "agenda.delete", DELETE);
    assert!(w.runner.jobs().is_empty(), "must not run before approval");
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(stored.state, ApprovalState::Pending);
    assert_eq!(stored.reasons, vec!["irreversible".to_string()]);
    assert!(
        stored
            .card
            .0
            .contains(&format!("\"nonce\":\"{}\"", nonce.0))
    );
    assert!(stored.card.0.contains("walk"));
    assert_eq!(awaiting(w.call("c1", "agenda.delete", DELETE)).0, nonce.0);
    w.decide(&nonce, "local:owner", true).unwrap();
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert!(
        w.ledger
            .woken()
            .iter()
            .any(|(run, call)| run.0 == "r1" && call.0 == "c1")
    );
    assert_eq!(
        w.runner.jobs().len(),
        0,
        "deciding alone must not execute the tool"
    );
    assert_eq!(done(w.call("c1", "agenda.delete", DELETE)), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(w.runner.jobs()[0].args, DELETE);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    assert_eq!(done(w.call("c1", "agenda.delete", DELETE)), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1, "replay must not re-execute");
    w.decide(&nonce, "local:owner", true).unwrap();
    assert!(matches!(
        w.decide(&nonce, "local:owner", false),
        Err(Error::Conflict)
    ));
}

#[test]
fn rejects_foreign_approver() {
    let w = World::new();
    let nonce = first_approval(&w, "c1", "agenda.delete", DELETE);
    assert!(matches!(
        w.decide(&nonce, "local:alice", true),
        Err(Error::Denied)
    ));
    assert!(matches!(
        w.decide(&nonce, "local:mallory", true),
        Err(Error::Denied)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    assert!(
        w.audit
            .verdicts()
            .iter()
            .any(|(a, v)| a == "approval.decide" && v == "actor_mismatch")
    );
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Ok(CallResult::Awaiting(_))
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_service_principal_can_never_approve_or_be_the_approver() {
    let w = World::new();
    let nonce = first_approval(&w, "c1", "agenda.delete", DELETE);
    assert!(matches!(
        w.decide(&nonce, "service:morning", true),
        Err(Error::Denied)
    ));
    for (run, approver) in [("r2", None), ("r3", Some("service:morning"))] {
        let mut ctx = run_context(
            "personal",
            run,
            1,
            "service:morning",
            "main",
            &["calendar.delete"],
        );
        ctx.approver = approver.map(|a| PrincipalId(a.into()));
        w.ledger.add_run(ctx);
        let result = w.call_on(&w.broker, run, 1, "c1", "agenda.delete", DELETE);
        assert!(matches!(result, Err(Error::Denied)), "{run}");
    }
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
fn a_scheduled_run_asks_the_trigger_approver_not_the_service() {
    let w = World::new();
    let mut ctx = run_context(
        "personal",
        "r2",
        1,
        "service:morning",
        "main",
        &["calendar.delete"],
    );
    ctx.approver = Some(PrincipalId("local:alice".into()));
    ctx.conversation.kind = bot_core::state::ConversationKind::Scheduled;
    ctx.trigger = Some(bot_core::state::TriggerRef {
        agent: AgentId("main".into()),
        id: "morning".into(),
    });
    w.ledger.add_run(ctx);
    let nonce = awaiting(w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE));
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(stored.approval.requester.0, "service:morning");
    assert_eq!(stored.approval.approver.0, "local:alice");
    assert!(matches!(
        w.decide(&nonce, "service:morning", true),
        Err(Error::Denied)
    ));
    assert!(matches!(
        w.decide(&nonce, "local:owner", true),
        Err(Error::Denied)
    ));
    w.decide(&nonce, "local:alice", true).unwrap();
    done(w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn rejects_stale_approval() {
    let w = World::new();
    let old = first_approval(&w, "c1", "agenda.delete", DELETE);
    w.ledger.bump_generation("r1");
    assert!(
        matches!(w.decide(&old, "local:owner", true), Err(Error::Stale)),
        "stale button must be rejected"
    );
    assert_eq!(w.ledger.approval_state(&old), ApprovalState::Pending);
    let new = awaiting(w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE));
    assert_ne!(new.0, old.0);
    assert_eq!(w.ledger.approval_state(&old), ApprovalState::Superseded);
    assert!(matches!(
        w.decide(&old, "local:owner", true),
        Err(Error::Stale)
    ));
    w.decide(&new, "local:owner", true).unwrap();
    done(w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE));
    assert_eq!(
        w.runner.jobs().len(),
        1,
        "new approval must be consumed once"
    );
    assert_eq!(w.ledger.approval_state(&new), ApprovalState::Consumed);
}

#[test]
fn a_risk_upgrade_while_waiting_voids_the_old_button() {
    let w = World::new();
    w.publish(base("r0").approve(&["calendar.write"]).build());
    w.ledger.add_run({
        let mut ctx = owner_run("r9", 1);
        ctx.work.revision = Revision("r0".into());
        ctx
    });
    let old = awaiting(w.call_on(&w.broker, "r9", 1, "c1", "agenda.update", DELETE));
    assert_eq!(
        w.ledger.stored_approval(&old).reasons,
        vec!["policy".to_string()]
    );
    let mut upgraded = tool(Risk::Irreversible, &["calendar.write"]);
    upgraded.revision = Revision("t2".into());
    w.publish(
        base("r2")
            .approve(&["calendar.write"])
            .tool("main", "agenda.update", upgraded)
            .build(),
    );
    assert!(
        matches!(w.decide(&old, "local:owner", true), Err(Error::Stale)),
        "stale button must be rejected"
    );
    assert_eq!(w.ledger.approval_state(&old), ApprovalState::Superseded);
    assert_eq!(
        w.ledger.woken(),
        vec![(RunId("r9".into()), CallId("c1".into()))]
    );
    assert!(matches!(
        w.decide(&old, "local:owner", true),
        Err(Error::Stale)
    ));
    assert_eq!(w.ledger.woken().len(), 1);
    let new = awaiting(w.call_on(&w.broker, "r9", 1, "c1", "agenda.update", DELETE));
    assert_ne!(new.0, old.0);
    assert_eq!(
        w.ledger.stored_approval(&new).approval.risk,
        Risk::Irreversible
    );
    assert_eq!(
        w.ledger.stored_approval(&new).reasons,
        vec!["irreversible".to_string()]
    );
    w.decide(&new, "local:owner", true).unwrap();
    done(w.call_on(&w.broker, "r9", 1, "c1", "agenda.update", DELETE));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn conditions_that_change_after_approval_void_it() {
    let w = World::new();
    let old = first_approval(&w, "c1", "mail.send", "bob|hi");
    w.decide(&old, "local:owner", true).unwrap();
    w.ledger.set_private_recall("r1");
    let new = awaiting(w.call("c1", "mail.send", "bob|hi"));
    assert_ne!(new.0, old.0);
    assert_eq!(w.ledger.approval_state(&old), ApprovalState::Superseded);
    assert_eq!(
        w.ledger.stored_approval(&new).reasons,
        vec!["irreversible".to_string(), "private_recall".to_string()]
    );
    assert!(w.runner.jobs().is_empty());
    w.decide(&new, "local:owner", true).unwrap();
    done(w.call("c1", "mail.send", "bob|hi"));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn an_installed_image_change_after_approval_voids_it() {
    let w = World::new();
    let old = first_approval(&w, "c1", "agenda.delete", DELETE);
    w.decide(&old, "local:owner", true).unwrap();
    w.runner.set_image("demo-agenda", Some("img2"));
    let new = awaiting(w.call("c1", "agenda.delete", DELETE));
    assert_ne!(new.0, old.0);
    assert!(
        w.ledger
            .stored_approval(&new)
            .approval
            .tool_revision
            .0
            .ends_with("@img2")
    );
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn an_expired_approval_blocks_execution() {
    let w = World::new();
    let nonce = first_approval(&w, "c1", "agenda.delete", DELETE);
    w.decide(&nonce, "local:owner", true).unwrap();
    w.clock.advance(Duration::from_secs(31 * 60));
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(w.runner.jobs().is_empty());
    assert!(
        w.audit
            .verdicts()
            .iter()
            .any(|(_, v)| v == "approval_expired")
    );
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Denied));
    let late = first_approval(&w, "c2", "agenda.delete", DELETE);
    w.clock.advance(Duration::from_secs(31 * 60));
    assert!(matches!(
        w.decide(&late, "local:owner", true),
        Err(Error::Stale)
    ));
}

#[test]
fn a_denied_or_revoked_approval_is_final() {
    let w = World::new();
    let n1 = first_approval(&w, "c1", "agenda.delete", DELETE);
    w.decide(&n1, "local:owner", false).unwrap();
    w.decide(&n1, "local:owner", false).unwrap();
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    let n2 = first_approval(&w, "c2", "agenda.delete", DELETE);
    w.decide(&n2, "local:owner", true).unwrap();
    w.decide(&n2, "local:owner", false).unwrap();
    assert_eq!(w.ledger.approval_state(&n2), ApprovalState::Revoked);
    assert!(matches!(
        w.call("c2", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(matches!(
        w.decide(&n2, "local:owner", true),
        Err(Error::Stale)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn permission_revoked_before_execution_is_rejected() {
    let w = World::new();
    let nonce = first_approval(&w, "c1", "agenda.delete", DELETE);
    w.decide(&nonce, "local:owner", true).unwrap();
    let lowered: Vec<&str> = ALL
        .iter()
        .copied()
        .filter(|s| *s != "calendar.delete")
        .collect();
    w.publish(base("r2").role("owner", &lowered).build());
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(
        w.ledger.approval_state(&nonce),
        ApprovalState::Approved,
        "approval must remain unconsumed"
    );
    assert!(w.audit.verdicts().iter().any(|(_, v)| v == "scope_missing"));
}

#[test]
fn changed_arguments_cannot_reuse_an_approval() {
    let w = World::new();
    let old = first_approval(&w, "c1", "agenda.delete", DELETE);
    w.decide(&old, "local:owner", true).unwrap();
    assert!(matches!(
        w.call("c1", "agenda.delete", "{\"event\":\"run\"}"),
        Err(Error::Conflict)
    ));
    let new = awaiting(w.call("c2", "agenda.delete", "{\"event\":\"run\"}"));
    assert_ne!(new.0, old.0);
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_consumed_approval_cannot_start_a_second_time() {
    let w = World::new();
    let nonce = first_approval(&w, "c1", "agenda.delete", DELETE);
    w.decide(&nonce, "local:owner", true).unwrap();
    done(w.call("c1", "agenda.delete", DELETE));
    use bot_core::ledger::*;
    let stored = w.ledger.stored_approval(&nonce);
    let start = |call: &str| StartCall {
        checked_revision: Revision("r1".into()),
        risk: bot_core::state::Risk::Read,
        tool_revision: Revision("test".into()),
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(1),
        },
        call: CallId(call.into()),
        approval: Some((stored.approval.clone(), stored.reasons.clone())),
        private_recall: false,
        owner: "x".into(),
    };
    assert!(matches!(
        block_on(w.ledger.start(start("c1"))),
        Err(Error::Conflict)
    ));
    block_on(w.ledger.begin(
        Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(1),
        },
        CallId("c2".into()),
        ToolId("agenda.delete".into()),
        [9; 32],
    ))
    .unwrap();
    assert!(matches!(
        block_on(w.ledger.start(start("c2"))),
        Err(Error::Denied)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn requires_approval_after_private_recall() {
    let w = World::new();
    done(w.call("c1", "web.search", "weather"));
    assert_eq!(w.runner.jobs().len(), 1);
    w.ledger.set_private_recall("r1");
    let n2 = awaiting(w.call("c2", "web.search", "my doctor appointment"));
    assert_eq!(w.runner.jobs().len(), 1, "no external call before approval");
    assert_eq!(
        w.ledger.stored_approval(&n2).reasons,
        vec!["private_recall".to_string()]
    );
    assert_eq!(w.ledger.stored_approval(&n2).approval.risk, Risk::Read);
    assert!(
        w.ledger
            .stored_approval(&n2)
            .card
            .0
            .contains("my doctor appointment")
    );
    w.decide(&n2, "local:owner", true).unwrap();
    done(w.call("c2", "web.search", "my doctor appointment"));
    assert_eq!(w.runner.jobs().len(), 2);
    assert_eq!(w.runner.jobs()[1].args, "my doctor appointment");
    let n3 = awaiting(w.call("c3", "web.search", "my doctor appointment"));
    assert_ne!(n3.0, n2.0);
    let rebooted = w.reboot();
    assert!(matches!(
        w.call_on(&rebooted, "r1", 1, "c4", "web.search", "q"),
        Ok(CallResult::Awaiting(_))
    ));
    let mut child = run_context("personal", "r5", 1, "local:owner", "main/research", &ALL);
    child.root = RunId("r1".into());
    w.ledger.add_run(child);
    assert!(matches!(
        w.call_on(&w.broker, "r5", 1, "c9", "web.search", "q"),
        Ok(CallResult::Awaiting(_))
    ));
    assert_eq!(w.runner.jobs().len(), 2);
}

#[test]
fn an_explicit_recipient_needs_its_own_approval_and_changed_content_voids_it() {
    let w = World::new();
    let n1 = first_approval(&w, "c1", "mail.send", "bob|hi");
    let n2 = first_approval(&w, "c2", "mail.send", "carol|hi");
    assert_ne!(n1.0, n2.0);
    assert!(
        w.runner.jobs().is_empty(),
        "must not send before explicit approval"
    );
    let card = w.ledger.stored_approval(&n1).card.0;
    assert!(
        card.contains(&format!(r#""{LABEL_RECIPIENT}: \"bob\"""#))
            && card.contains(&format!(r#""{LABEL_CONTENT}: \"hi\"""#))
    );
    assert_eq!(
        w.ledger.stored_approval(&n1).approval.risk,
        Risk::Irreversible
    );
    w.decide(&n1, "local:owner", true).unwrap();
    assert!(matches!(
        w.call("c1", "mail.send", "bob|bye"),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.call("c3", "mail.send", "bob|bye"),
        Ok(CallResult::Awaiting(_))
    ));
    assert!(w.runner.jobs().is_empty());
    done(w.call("c1", "mail.send", "bob|hi"));
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(
        w.runner.jobs()[0].args,
        "bob|hi",
        "must execute with exact approved args"
    );
}

#[test]
fn a_destination_that_cannot_be_determined_sends_nothing() {
    let w = World::new();
    assert!(matches!(
        w.call("c1", "mail.send", "no-separator"),
        Err(Error::Invalid)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_card_too_large_to_show_in_full_is_refused_for_good() {
    let w = World::new();
    let huge = format!("bob|{}", "★".repeat(4000));
    assert!(matches!(
        w.call("c1", "mail.send", &huge),
        Err(Error::Denied)
    ));
    assert!(matches!(
        w.call("c1", "mail.send", &huge),
        Err(Error::Denied)
    ));
    assert!(
        w.audit
            .verdicts()
            .iter()
            .any(|(_, v)| v == "card_too_large")
    );
    assert!(w.runner.jobs().is_empty());
}
