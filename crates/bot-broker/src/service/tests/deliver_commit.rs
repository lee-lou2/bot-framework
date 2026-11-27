//! Post-commit receipt replay, active-set fencing, and `private_recall` tests for scheduled delivery.

use super::{
    deliver::{RUN, TEXT, attempt_of, deliver_world, draft, release, release_on},
    *,
};
use bot_core::{
    id::ClaimEpoch,
    ledger::{ApprovalState, CallLedger},
    schedule::{DELIVER_CALL, ScheduleDelivery, ScheduledReplies},
};

use super::schedule::OWNER;

#[test]
fn a_lost_commit_response_is_retried_into_the_same_receipt_without_a_second_send() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.replies.lose_response(true);
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert_eq!(w.replies.outbox().len(), 1, "commit already succeeded");
    w.replies.lose_response(false);
    assert_eq!(done(release(&w)), draft(TEXT).queued_receipt().0);
    assert_eq!(w.replies.outbox().len(), 1);
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.replies.fail_with(Some(Error::Unavailable));
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert!(w.replies.outbox().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    w.replies.fail_with(None);
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
}

#[test]
fn the_commit_ends_the_run_so_a_retry_reads_the_receipt_through_the_dedicated_port() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    let receipt = done(release(&w));
    let attempt = attempt_of(&w, RUN);
    assert!(matches!(
        block_on(w.ledger.context(attempt.clone())),
        Err(Error::Stale)
    ));
    assert!(matches!(
        block_on(
            w.ledger
                .lookup(attempt.clone(), CallId(DELIVER_CALL.into()))
        ),
        Err(Error::Stale)
    ));
    // Retries read the stored receipt via `ScheduledReplies::receipt` without re-reading draft or resending.
    w.replies.set_draft(RUN, draft("changed content later"));
    assert_eq!(done(release(&w)), receipt);
    assert_eq!(done(release(&w)), receipt);
    assert_eq!(w.replies.outbox().len(), 1);
}

#[test]
fn only_the_final_claim_and_the_registration_set_read_the_receipt() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    let receipt = done(release(&w));
    let by = |edit: &dyn Fn(&mut Attempt)| {
        let mut attempt = attempt_of(&w, RUN);
        edit(&mut attempt);
        block_on(w.broker.release(attempt))
    };
    assert!(matches!(
        by(&|a| a.claim = Some(ClaimEpoch(99))),
        Err(Error::Stale)
    ));
    assert!(matches!(by(&|a| a.claim = None), Err(Error::Stale)));
    assert!(matches!(
        by(&|a| a.generation = Generation(2)),
        Err(Error::Stale)
    ));
    assert!(matches!(
        by(&|a| a.profile = ProfileId("other".into())),
        Err(Error::Denied)
    ));
    assert_eq!(done(by(&|_| {})), receipt);
    w.replies.set_active_set("set-b");
    assert!(matches!(by(&|_| {}), Err(Error::Stale)));
    w.replies.set_active_set("set-a");
    assert_eq!(done(by(&|_| {})), receipt);
    w.replies.clear_active_set();
    assert!(matches!(by(&|_| {}), Err(Error::Stale)));
    assert!(matches!(
        block_on(w.replies.receipt(attempt_of(&w, RUN))),
        Err(Error::Stale)
    ));
    assert_eq!(w.replies.outbox().len(), 1);
}

#[test]
fn before_the_registration_there_is_no_receipt_and_the_general_claim_check_decides_authority() {
    let w = deliver_world();
    assert!(matches!(
        block_on(w.replies.receipt(attempt_of(&w, RUN))),
        Ok(None)
    ));
    let mut stale = attempt_of(&w, RUN);
    stale.claim = Some(ClaimEpoch(99));
    assert!(matches!(
        block_on(w.replies.receipt(stale.clone())),
        Ok(None)
    ));
    assert!(matches!(
        block_on(w.broker.release(stale)),
        Err(Error::Stale)
    ));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    let bare = w.bare();
    assert!(matches!(
        release_on(&w, &bare, RUN),
        Err(Error::Unavailable)
    ));
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn a_delivery_after_private_memory_was_read_keeps_the_private_recall_reason() {
    let w = deliver_world();
    w.ledger.set_private_recall(RUN);
    let nonce = awaiting(release(&w));
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(
        stored.reasons,
        vec!["irreversible".to_string(), "private_recall".to_string()]
    );
    assert!(
        stored
            .card
            .0
            .contains("Reason: irreversible, private_recall"),
        "{}",
        stored.card.0
    );
    w.decide(&nonce, OWNER, true).unwrap();
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    assert_eq!(
        w.ledger.stored_approval(&nonce).reasons,
        vec!["irreversible".to_string()]
    );
}
