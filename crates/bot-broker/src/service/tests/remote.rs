//! Broker rules for remote (Streamable HTTP) MCP servers: definition mismatch, missing credentials, and unprovable recovery.
use super::*;
use crate::ports::Ran;
use bot_core::ledger::{ApprovalState, CallPhase};

const DELETE: &str = "{\"event\":\"walk\"}";

fn approve(w: &World, call: &str) -> ApprovalId {
    let nonce = awaiting(w.call(call, "agenda.delete", DELETE));
    w.decide(&nonce, "local:owner", true).unwrap();
    nonce
}

fn audited(w: &World, code: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("call.refuse".into(), code.into()))
}

#[test]
fn a_definition_mismatch_after_approval_is_refused_with_a_fixed_code_and_closes_the_call_for_good()
{
    for code in ["tool_definition_changed", "tool_missing"] {
        let w = World::new();
        let nonce = approve(&w, "c1");
        w.runner.script(Ran::Refused(code));
        let result = w.call("c1", "agenda.delete", DELETE);
        assert!(matches!(result, Err(Error::Denied)), "{code}: {result:?}");
        assert_eq!(w.runner.jobs().len(), 1, "{code}: call attempted once");
        assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Denied));
        assert!(audited(&w, code), "{code}: {:?}", w.audit.verdicts());
        // Consumed approval stays consumed; replays neither execute nor reopen approval.
        assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
        let again = w.call("c1", "agenda.delete", DELETE);
        assert!(matches!(again, Err(Error::Denied)), "{code}: {again:?}");
        assert_eq!(w.runner.jobs().len(), 1, "{code}: zero automatic retries");
    }
}

#[test]
fn a_refused_read_is_denied_too_and_nothing_is_retried() {
    let w = World::new();
    w.runner.script(Ran::Refused("tool_definition_changed"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(audited(&w, "tool_definition_changed"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn a_server_bound_credential_the_store_lacks_refuses_a_new_call_before_any_card_or_run() {
    let w = World::new();
    w.runner.bind_server_secret("demo-agenda", "docs-token");
    w.runner.miss_secret("docs-token");
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(audited(&w, "secret_missing"));
    assert!(!w.ledger.has_call("r1", "c1"), "must not record call");
    assert!(w.ledger.audit_log().is_empty(), "must not open approval");
    assert!(w.runner.jobs().is_empty());
    // Checked the server-bound secret in the profile's namespace.
    assert_eq!(
        w.runner.secret_checks(),
        vec![(ProfileId("personal".into()), vec!["docs-token".to_owned()])]
    );
    // Once restored, the same call ID proceeds to approval.
    w.runner.restore_secret("docs-token");
    awaiting(w.call("c1", "agenda.delete", DELETE));
    assert!(w.ledger.has_call("r1", "c1"));
}

#[test]
fn a_credential_bound_to_another_server_is_not_asked_for() {
    let w = World::new();
    w.runner.bind_server_secret("other-server", "docs-token");
    w.runner.miss_secret("docs-token");
    awaiting(w.call("c1", "agenda.delete", DELETE));
    assert!(
        w.runner.secret_checks().is_empty(),
        "must not query store when server has no bound or declared secrets"
    );
}

#[test]
fn closing_the_connection_is_not_proof_that_a_remote_call_ended() {
    // Remote server: even reads stay held when termination of the prior call is unproven.
    let w = World::new();
    w.runner.reap_proves_nothing_for("demo-agenda");
    w.leave_started("r1", "c1", "agenda.list", "{}", 1);
    w.ledger.bump_generation("r1");
    assert!(is_held(&w.call_on(
        &w.broker,
        "r1",
        2,
        "c1",
        "agenda.list",
        "{}"
    )));
    assert_eq!(w.runner.reaped(), vec!["r1.1.c1".to_string()]);
    assert!(w.runner.jobs().is_empty(), "must not re-run");
    match w.ledger.phase("r1", "c1") {
        CallPhase::Held { reason, .. } => assert_eq!(reason, "previous_process_unconfirmed"),
        other => panic!("{other:?}"),
    }
    // Local server: confirmed reap allows retrying a read.
    let local = World::new();
    local.leave_started("r1", "c1", "agenda.list", "{}", 1);
    local.ledger.bump_generation("r1");
    done(local.call_on(&local.broker, "r1", 2, "c1", "agenda.list", "{}"));
    assert_eq!(local.runner.jobs().len(), 1);
}

#[test]
fn an_irreversible_remote_write_waits_for_the_same_single_approval_as_a_local_one() {
    let w = World::new();
    w.runner.bind_server_secret("demo-agenda", "docs-token");
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty(), "zero runs before approval");
    assert_eq!(
        w.ledger.stored_approval(&nonce).reasons,
        vec!["irreversible".to_string()]
    );
    w.decide(&nonce, "local:owner", true).unwrap();
    done(w.call("c1", "agenda.delete", DELETE));
    assert_eq!(
        w.runner.jobs().len(),
        1,
        "must run once with a single approval"
    );
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
}
