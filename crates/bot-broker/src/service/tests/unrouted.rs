//! Explicit-destination route verification outcomes: confirmed removal/re-pointing (`delivery_unrouted` with notice), owner denial/revocation (`delivery_rejected` without notice), and unconfirmed registry outages (`Unavailable` retry).

use super::{
    deliver::{RUN, deliver_world, release},
    destination::{slack_world, with_destination},
    schedule::{OWNER, create},
    *,
};
use bot_core::{
    ledger::{ApprovalState, CallPhase},
    schedule::{DELIVER_CALL, DeliveryFence, ScheduledReplies, UNROUTED_NOTICE, UnsentReason},
    state::ConversationKind,
};

/// Runs `action` only on the first `begin` call so retries are not intercepted again.
fn after_begin_once(w: &World, action: impl Fn() + Send + Sync + 'static) {
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    w.ledger.set_after_begin(move || {
        if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            action();
        }
    });
}

fn unrouted() -> String {
    UnsentReason::Unrouted.receipt().0
}

fn rejected() -> String {
    UnsentReason::Rejected.receipt().0
}

#[test]
fn the_first_card_is_never_opened_when_the_recipient_leaves_between_the_checks_and_the_card() {
    // Schedule creation: recipient unregistered after `begin` -> no card, no schedule.
    let w = slack_world();
    let routes = w.routes.clone();
    w.ledger.set_after_begin(move || routes.unregister("T1/C1"));
    assert!(matches!(
        create(&w, "c1", &with_destination("report")),
        Err(Error::Denied)
    ));
    assert!(w.ledger.pending_approval("r1", "c1").is_none(), "no card");
    assert!(matches!(
        w.ledger.phase("r1", "c1"),
        CallPhase::Open { approval: None }
    ));
    assert_eq!(w.schedules.count(), 0);
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.refuse".into(), "destination_unregistered".into()))
    );
    // Scheduled delivery: same race closes as unrouted with one notice and zero outbox entries.
    let w = deliver_world();
    let routes = w.routes.clone();
    w.ledger
        .set_after_begin(move || routes.unregister("channel-a"));
    assert_eq!(done(release(&w)), unrouted());
    assert!(
        w.ledger.pending_approval(RUN, DELIVER_CALL).is_none(),
        "no card"
    );
    assert!(
        w.ledger.approval_ids(RUN).is_empty(),
        "no approval should be opened"
    );
    assert_eq!(
        w.replies.notices(),
        vec![(RunId(RUN.into()), UNROUTED_NOTICE.to_owned())]
    );
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn an_unconfirmed_registry_retries_the_same_occurrence_at_every_step_and_nothing_is_consumed_or_closed()
 {
    // Outage at start: retry without recording, opening a card, or emitting a notice.
    let w = deliver_world();
    w.routes.outage(true);
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    w.routes.outage(false);
    // Outage right before opening card: recovery opens the card on retry.
    let routes = w.routes.clone();
    after_begin_once(&w, move || routes.outage(true));
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert!(w.ledger.approval_ids(RUN).is_empty(), "no card");
    assert!(matches!(
        w.ledger.phase(RUN, DELIVER_CALL),
        CallPhase::Open { approval: None }
    ));
    w.routes.outage(false);
    let nonce = awaiting(release(&w));
    // Outage on approval click: button stays pending for retry.
    w.routes.outage(true);
    assert!(matches!(
        w.decide(&nonce, OWNER, true),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    w.routes.outage(false);
    w.decide(&nonce, OWNER, true).unwrap();
    // Outage right before consuming approval: nothing is consumed or closed; recovery sends once.
    let routes = w.routes.clone();
    after_begin_once(&w, move || routes.outage(true));
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert!(w.replies.outbox().is_empty() && w.replies.notices().is_empty());
    w.routes.outage(false);
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
    assert!(w.replies.notices().is_empty());
    // Same behavior for schedule creation across start, click, and consumption.
    let w = slack_world();
    w.routes.outage(true);
    assert!(matches!(
        create(&w, "c1", &with_destination("report")),
        Err(Error::Unavailable)
    ));
    assert!(!w.ledger.has_call("r1", "c1"));
    w.routes.outage(false);
    let nonce = awaiting(create(&w, "c1", &with_destination("report")));
    w.routes.outage(true);
    assert!(matches!(
        w.decide(&nonce, OWNER, true),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    assert_eq!(w.schedules.count(), 0);
    w.routes.outage(false);
    w.decide(&nonce, OWNER, true).unwrap();
    let routes = w.routes.clone();
    after_begin_once(&w, move || routes.outage(true));
    assert!(matches!(
        create(&w, "c1", &with_destination("report")),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.schedules.count(), 0);
    w.routes.outage(false);
    done(create(&w, "c1", &with_destination("report")));
    assert_eq!(w.schedules.count(), 1);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
}

#[test]
fn a_recipient_re_pointed_to_another_real_destination_ends_as_unrouted_like_a_removed_one() {
    let w = deliver_world();
    w.routes.unregister("channel-a");
    w.routes.register(
        "personal",
        "team",
        ConversationKind::Channel,
        "channel-a",
        "T9/C9",
    );
    assert_eq!(done(release(&w)), unrouted());
    assert_eq!(w.replies.notices().len(), 1);
    assert!(w.replies.outbox().is_empty());
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.routes.unregister("channel-a");
    w.routes.register(
        "personal",
        "team",
        ConversationKind::Channel,
        "channel-a",
        "T9/C9",
    );
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert_eq!(w.replies.notices().len(), 1);
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn an_owner_rejection_and_a_removed_recipient_end_with_different_receipts_and_only_the_second_leaves_a_notice()
 {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, false).unwrap();
    assert_eq!(done(release(&w)), rejected());
    assert!(
        w.replies.notices().is_empty(),
        "owner rejection must not emit a notice"
    );
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.decide(&nonce, OWNER, false).unwrap();
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Revoked);
    assert_eq!(done(release(&w)), rejected());
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    let w = deliver_world();
    w.routes.unregister("channel-a");
    assert_eq!(done(release(&w)), unrouted());
    let notices = w.replies.notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].1, UNROUTED_NOTICE);
    assert!(!notices[0].1.contains("channel-a") && !notices[0].1.contains(super::deliver::TEXT));
    assert_ne!(unrouted(), rejected());
    // Re-settling a finished occurrence with a different reason conflicts.
    let again = block_on(w.replies.finish_unsent(
        DeliveryFence::Claim(super::deliver::attempt_of(&w, RUN)),
        super::deliver::draft(super::deliver::TEXT).target,
        UnsentReason::Rejected,
    ));
    assert!(matches!(again, Err(Error::Conflict)));
}

#[test]
fn after_a_new_activation_a_removed_recipient_is_told_apart_from_a_stale_draft() {
    // Unregistered recipient settles as unrouted rather than being masked by a stale-set Conflict.
    let w = deliver_world();
    w.replies.set_active_set("set-b");
    w.routes.unregister("channel-a");
    assert_eq!(done(release(&w)), unrouted());
    assert_eq!(w.replies.notices().len(), 1);
    assert!(w.replies.outbox().is_empty());
    let w = deliver_world();
    w.replies.set_active_set("set-b");
    assert!(matches!(release(&w), Err(Error::Conflict)));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    let w = deliver_world();
    w.replies.set_active_set("set-b");
    w.routes.outage(true);
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert!(w.replies.notices().is_empty());
    w.routes.outage(false);
    assert!(matches!(release(&w), Err(Error::Conflict)));
}

#[test]
fn a_decide_broker_without_the_replies_port_cannot_end_an_unrouted_occurrence_and_keeps_the_button_valid()
 {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.routes.unregister("channel-a");
    // A decide-only broker without `ScheduledReplies` cannot settle unrouted and leaves the button pending.
    let decider = w.bare().with_routes(w.routes.clone());
    let result = block_on(decider.decide(nonce.clone(), PrincipalId(OWNER.into()), true));
    assert!(matches!(result, Err(Error::Unavailable)), "{result:?}");
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    assert!(w.replies.notices().is_empty());
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.replies.notices().len(), 1);
}

#[test]
fn a_click_by_anyone_else_never_ends_an_occurrence() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.routes.unregister("channel-a");
    assert!(matches!(
        w.decide(&nonce, "local:alice", true),
        Err(Error::Denied)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
}
