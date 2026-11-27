//! Owner-decision precedence tests ensuring recorded denial or revocation beats later route drift.

use super::{
    deliver::{RUN, deliver_world, release},
    schedule::OWNER,
    *,
};
use bot_core::{
    ledger::{ApprovalState, CallLedger, Decision},
    schedule::{DELIVER_CALL, DeliveryFence, ScheduledReplies, UnsentReason},
};

fn rejected() -> String {
    UnsentReason::Rejected.receipt().0
}

fn unrouted() -> String {
    UnsentReason::Unrouted.receipt().0
}

#[derive(Clone, Copy, Debug)]
enum Verdict {
    Denied,
    Revoked,
}

impl Verdict {
    fn record(self, w: &World, nonce: &ApprovalId) {
        if matches!(self, Verdict::Revoked) {
            w.decide(nonce, OWNER, true).unwrap();
        }
        w.decide(nonce, OWNER, false).unwrap();
        let state = match self {
            Verdict::Denied => ApprovalState::Denied,
            Verdict::Revoked => ApprovalState::Revoked,
        };
        assert_eq!(w.ledger.approval_state(nonce), state);
    }
}

#[derive(Clone, Copy, Debug)]
enum Registry {
    Gone,
    Transient,
    Same,
}

impl Registry {
    fn apply(self, w: &World) {
        match self {
            Registry::Gone => w.routes.unregister("channel-a"),
            Registry::Transient => w.routes.outage(true),
            Registry::Same => {}
        }
    }
}

const VERDICTS: [Verdict; 2] = [Verdict::Denied, Verdict::Revoked];
const REGISTRIES: [Registry; 2] = [Registry::Gone, Registry::Transient];
const RACE_REGISTRIES: [Registry; 3] = [Registry::Gone, Registry::Transient, Registry::Same];

fn assert_rejected_quietly(w: &World, nonce: &ApprovalId, state: ApprovalState, what: &str) {
    assert!(
        w.replies.notices().is_empty(),
        "{what}: rejection must not enqueue notices"
    );
    assert!(w.replies.outbox().is_empty(), "{what}: nothing was sent");
    assert_eq!(w.ledger.approval_state(nonce), state, "{what}");
}

#[test]
fn a_recorded_owner_decision_wins_over_a_removed_or_unreachable_recipient() {
    for verdict in VERDICTS {
        for registry in REGISTRIES {
            let what = format!("{verdict:?} x {registry:?}");
            let w = deliver_world();
            let nonce = awaiting(release(&w));
            verdict.record(&w, &nonce);
            registry.apply(&w);
            let state = w.ledger.approval_state(&nonce);
            assert_eq!(done(release(&w)), rejected(), "{what}");
            assert_rejected_quietly(&w, &nonce, state, &what);
            assert_eq!(done(release(&w)), rejected(), "{what}: retry");
            assert!(
                w.replies.notices().is_empty(),
                "{what}: still no notices after retry"
            );
        }
    }
}

#[test]
fn an_owner_decision_also_wins_when_the_draft_was_frozen_under_an_older_set() {
    for verdict in VERDICTS {
        for registry in [None, Some(Registry::Gone), Some(Registry::Transient)] {
            let what = format!("{verdict:?} x {registry:?} x new set");
            let w = deliver_world();
            let nonce = awaiting(release(&w));
            verdict.record(&w, &nonce);
            w.replies.set_active_set("set-b");
            if let Some(registry) = registry {
                registry.apply(&w);
            }
            let state = w.ledger.approval_state(&nonce);
            assert_eq!(done(release(&w)), rejected(), "{what}");
            assert_rejected_quietly(&w, &nonce, state, &what);
        }
    }
}

#[test]
fn without_an_owner_decision_the_registry_state_still_decides() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.routes.unregister("channel-a");
    assert_eq!(done(release(&w)), unrouted());
    assert_eq!(w.replies.notices().len(), 1);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.routes.outage(true);
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert!(w.replies.notices().is_empty());
}

#[test]
fn a_revocation_that_lands_after_the_approval_was_read_still_wins_over_the_registry() {
    for registry in RACE_REGISTRIES {
        let what = format!("revoked x {registry:?}");
        let w = deliver_world();
        let nonce = awaiting(release(&w));
        w.decide(&nonce, OWNER, true).unwrap();
        let (routes, ledger, owner) = (w.routes.clone(), w.ledger.clone(), nonce.clone());
        // Route checks in `release`: (1) after draft, (2) before consume; trigger race at (2).
        w.routes.on_nth_ask(w.routes.asked() + 2, move || {
            block_on(ledger.decide(Decision {
                nonce: owner.clone(),
                actor: PrincipalId(OWNER.into()),
                allow: false,
            }))
            .unwrap();
            match registry {
                Registry::Gone => routes.unregister("channel-a"),
                Registry::Transient => routes.outage(true),
                Registry::Same => {}
            }
        });
        assert_eq!(done(release(&w)), rejected(), "{what}");
        assert!(
            w.replies.notices().is_empty() && w.replies.outbox().is_empty(),
            "{what}"
        );
        assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Revoked);
    }
}

#[test]
fn the_reply_store_settles_the_race_itself_a_stored_owner_decision_beats_an_unrouted_request() {
    for verdict in VERDICTS {
        let w = deliver_world();
        let nonce = awaiting(release(&w));
        verdict.record(&w, &nonce);
        let attempt = super::deliver::attempt_of(&w, RUN);
        let target = super::deliver::draft(super::deliver::TEXT).target;
        let receipt = block_on(w.replies.finish_unsent(
            DeliveryFence::Claim(attempt),
            target,
            UnsentReason::Unrouted,
        ))
        .unwrap();
        assert_eq!(receipt.0, rejected(), "{verdict:?}");
        assert!(w.replies.notices().is_empty(), "{verdict:?}: no notices");
        for reason in [UnsentReason::Unrouted, UnsentReason::Rejected] {
            let again = block_on(w.replies.finish_unsent(
                DeliveryFence::Claim(super::deliver::attempt_of(&w, RUN)),
                super::deliver::draft(super::deliver::TEXT).target,
                reason,
            ))
            .unwrap();
            assert_eq!(again.0, rejected(), "{verdict:?} retry {reason:?}");
        }
        assert!(w.replies.notices().is_empty());
        assert!(w.ledger.has_call(RUN, DELIVER_CALL));
    }
}

#[test]
fn a_click_that_races_with_a_stored_owner_decision_is_a_stale_button_and_the_resumed_attempt_rejects()
 {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, false).unwrap();
    let click = block_on(w.replies.finish_unsent(
        DeliveryFence::Approval {
            nonce: nonce.clone(),
            actor: PrincipalId(OWNER.into()),
        },
        super::deliver::draft(super::deliver::TEXT).target,
        UnsentReason::Unrouted,
    ));
    assert!(matches!(click, Err(Error::Stale)), "{click:?}");
    assert!(w.replies.notices().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Denied);
    w.routes.unregister("channel-a");
    assert_eq!(done(release(&w)), rejected());
    assert!(w.replies.notices().is_empty());
}

#[test]
fn a_rejection_without_a_stored_owner_decision_is_refused_and_changes_nothing() {
    let reject = |w: &World| {
        block_on(w.replies.finish_unsent(
            DeliveryFence::Claim(super::deliver::attempt_of(w, RUN)),
            super::deliver::draft(super::deliver::TEXT).target,
            UnsentReason::Rejected,
        ))
    };
    let w = deliver_world();
    assert!(matches!(reject(&w), Err(Error::Denied)), "no card");
    let nonce = awaiting(release(&w));
    assert!(matches!(reject(&w), Err(Error::Denied)), "pending");
    w.decide(&nonce, OWNER, true).unwrap();
    assert!(matches!(reject(&w), Err(Error::Denied)), "approved");
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
}

#[test]
fn a_rejected_consumption_with_a_stored_revocation_is_finished_as_rejected_and_other_denials_stay_denials()
 {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    let (ledger, owner) = (w.ledger.clone(), nonce.clone());
    w.routes.on_nth_ask(w.routes.asked() + 2, move || {
        block_on(ledger.decide(Decision {
            nonce: owner.clone(),
            actor: PrincipalId(OWNER.into()),
            allow: false,
        }))
        .unwrap();
    });
    assert_eq!(done(release(&w)), rejected());
    assert_eq!(done(release(&w)), rejected(), "retry returns same receipt");
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Revoked);
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    let (ledger, owner, replies) = (w.ledger.clone(), nonce.clone(), w.replies.clone());
    w.routes.on_nth_ask(w.routes.asked() + 2, move || {
        block_on(ledger.decide(Decision {
            nonce: owner.clone(),
            actor: PrincipalId(OWNER.into()),
            allow: false,
        }))
        .unwrap();
        replies.set_active_set("set-b");
    });
    assert_eq!(done(release(&w)), rejected());
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.replies.fail_with(Some(Error::Denied));
    assert!(matches!(release(&w), Err(Error::Denied)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
}
