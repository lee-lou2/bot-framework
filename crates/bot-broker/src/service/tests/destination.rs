//! Tests for explicit-destination schedules, canonical route registry checks, and delivery closure.

use super::{
    deliver::{RUN, deliver_world, release},
    schedule::{AT, OWNER, WITH_SCHEDULE, create, snapshot_with},
    *,
};
use bot_core::{
    ledger::ApprovalState,
    schedule::{DELIVER_CALL, UNROUTED_NOTICE, UnsentReason},
    state::{ConversationKind, Risk},
};

pub(super) const NAMED: &str = r##""destination":{"binding":"slack","conversation":{"kind":"channel","id":"#general","thread":"t1"}}"##;

pub(super) fn with_destination(instruction: &str) -> String {
    format!(r#"{{"at":"{AT}","instruction":"{instruction}",{NAMED}}}"#)
}

pub(super) fn slack_world() -> World {
    let w = World::new();
    w.publish(snapshot_with("r1").build());
    let mut ctx = run_context("personal", "r1", 1, OWNER, "main", &WITH_SCHEDULE);
    ctx.conversation.kind = ConversationKind::Direct;
    w.ledger.add_run(ctx);
    w.routes.register(
        "personal",
        "slack",
        ConversationKind::Channel,
        "#general",
        "T1/C1",
    );
    w
}

fn refused(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("schedule.refuse".into(), word.into()))
}

fn refused_late(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("call.refuse".into(), word.into()))
}

fn nothing_recorded(w: &World, call: &str) {
    assert!(!w.ledger.has_call("r1", call), "must not record call");
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn an_entrance_that_only_answers_in_place_a_missing_registry_or_an_unknown_recipient_creates_nothing()
 {
    for kind in [ConversationKind::Cli, ConversationKind::Http] {
        let w = slack_world();
        let mut ctx = run_context("personal", "r1", 1, OWNER, "main", &WITH_SCHEDULE);
        ctx.conversation.kind = kind;
        w.ledger.add_run(ctx);
        assert!(matches!(
            create(&w, "c1", &with_destination("report")),
            Err(Error::Denied)
        ));
        assert!(refused(&w, "destination_unsupported"));
        assert_eq!(w.routes.asked(), 0);
        nothing_recorded(&w, "c1");
    }
    let w = slack_world();
    let broker = w.bare().with_schedules(w.schedules.clone());
    assert!(matches!(
        w.call_on(
            &broker,
            "r1",
            1,
            "c1",
            "schedule.create",
            &with_destination("report")
        ),
        Err(Error::Denied)
    ));
    assert!(refused(&w, "destination_unsupported"));
    nothing_recorded(&w, "c1");
    let w = slack_world();
    let unknown = format!(
        r##"{{"at":"{AT}","instruction":"report","destination":{{"binding":"slack","conversation":{{"kind":"channel","id":"#random"}}}}}}"##
    );
    assert!(matches!(create(&w, "c1", &unknown), Err(Error::Denied)));
    assert!(refused(&w, "destination_unregistered"));
    assert_eq!(w.routes.asked(), 1);
    nothing_recorded(&w, "c1");
    let bad = format!(
        r#"{{"at":"{AT}","instruction":"report","destination":{{"binding":"slack","conversation":{{"kind":"cli","id":"x"}}}}}}"#
    );
    assert!(matches!(create(&w, "c2", &bad), Err(Error::Invalid)));
    assert_eq!(w.routes.asked(), 1);
    nothing_recorded(&w, "c2");
}

#[test]
fn a_registered_recipient_is_created_only_after_the_owner_approves_the_canonical_recipient_and_the_instruction()
 {
    // Explicit-destination creation is always irreversible even when policy requires no approvals.
    let w = slack_world();
    let nonce = awaiting(create(&w, "c1", &with_destination("send weekly report")));
    assert_eq!(w.schedules.count(), 0);
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(stored.approval.tool.0, "schedule.create");
    assert_eq!(stored.approval.tool_revision.0, "builtin-1");
    assert_eq!(stored.approval.risk, Risk::Irreversible);
    assert_eq!(stored.reasons, vec!["irreversible".to_string()]);
    let card = stored.card.0;
    assert!(card.contains("T1/C1") && card.contains("t1"), "{card}");
    assert!(card.contains("send weekly report"), "{card}");
    assert!(!card.contains("#general"), "{card}");
    w.decide(&nonce, OWNER, true).unwrap();
    let receipt = done(create(&w, "c1", &with_destination("send weekly report")));
    assert_eq!(w.schedules.count(), 1);
    let id = receipt.split('"').nth(3).expect("schedule ID").to_owned();
    let stored = w.schedules.destination(&id).expect("stored destination");
    assert_eq!(
        (
            stored.binding.0.as_str(),
            stored.conversation.id.as_str(),
            stored.conversation.thread.as_deref()
        ),
        ("slack", "T1/C1", Some("t1"))
    );
    assert_eq!(stored.conversation.kind, ConversationKind::Channel);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    assert_eq!(
        done(create(&w, "c1", &with_destination("send weekly report"))),
        receipt
    );
    assert_eq!(w.schedules.count(), 1);
    let plain = id_of(&done(create(
        &w,
        "c2",
        &crate::service::tests::schedule::at_args("here"),
    )));
    assert!(w.schedules.destination(&plain).is_none());
}

fn id_of(payload: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
    value["schedule"].as_str().expect("schedule").to_owned()
}

#[test]
fn private_memory_read_before_the_request_adds_its_reason_to_the_same_card() {
    let w = slack_world();
    w.ledger.set_private_recall("r1");
    let nonce = awaiting(create(&w, "c1", &with_destination("report")));
    assert_eq!(
        w.ledger.stored_approval(&nonce).reasons,
        vec!["irreversible".to_string(), "private_recall".to_string()]
    );
    w.decide(&nonce, OWNER, true).unwrap();
    done(create(&w, "c1", &with_destination("report")));
    assert_eq!(w.schedules.count(), 1);
}

#[test]
fn the_registry_is_read_again_at_the_decision_and_before_the_creation_approval_is_consumed() {
    let w = slack_world();
    let nonce = awaiting(create(&w, "c1", &with_destination("report")));
    w.routes.unregister("T1/C1");
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert_eq!(w.schedules.count(), 0);
    let w = slack_world();
    let nonce = awaiting(create(&w, "c1", &with_destination("report")));
    w.decide(&nonce, OWNER, true).unwrap();
    w.routes.unregister("T1/C1");
    assert!(matches!(
        create(&w, "c1", &with_destination("report")),
        Err(Error::Denied)
    ));
    assert!(refused(&w, "destination_unregistered"));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.schedules.count(), 0);
    w.routes.register(
        "personal",
        "slack",
        ConversationKind::Channel,
        "#general",
        "T1/C1",
    );
    done(create(&w, "c1", &with_destination("report")));
    assert_eq!(w.schedules.count(), 1);
    let w = slack_world();
    let nonce = awaiting(create(&w, "c1", &with_destination("report")));
    w.decide(&nonce, OWNER, true).unwrap();
    w.routes.unregister("T1/C1");
    w.routes.register(
        "personal",
        "slack",
        ConversationKind::Channel,
        "#general",
        "T9/C9",
    );
    assert!(matches!(
        create(&w, "c1", &with_destination("report")),
        Err(Error::Conflict)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn a_delivery_to_a_recipient_that_left_the_registry_gets_no_card_and_ends_as_unsent() {
    let w = deliver_world();
    w.routes.unregister("channel-a");
    let receipt = done(release(&w));
    assert_eq!(receipt, UnsentReason::Unrouted.receipt().0);
    assert!(
        !w.ledger.has_call(RUN, DELIVER_CALL),
        "must not record a delivery call when unrouted"
    );
    assert_eq!(
        w.replies.notices(),
        vec![(RunId(RUN.into()), UNROUTED_NOTICE.to_owned())]
    );
    assert!(w.replies.outbox().is_empty());
    assert_eq!(done(release(&w)), receipt);
    assert_eq!(w.replies.notices().len(), 1);
    let w = deliver_world();
    let broker = w.bare().with_replies(w.replies.clone());
    assert!(matches!(
        super::deliver::release_on(&w, &broker, RUN),
        Err(Error::Unavailable)
    ));
    w.routes.outage(true);
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    w.routes.outage(false);
    awaiting(release(&w));
}

#[test]
fn a_recipient_that_left_after_the_card_ends_the_occurrence_and_a_return_never_revives_the_old_approval()
 {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.routes.unregister("channel-a");
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert_eq!(w.replies.notices().len(), 1);
    assert!(w.replies.outbox().is_empty());
    assert_eq!(done(release(&w)), UnsentReason::Unrouted.receipt().0);
    assert_eq!(w.replies.notices().len(), 1);
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.routes.unregister("channel-a");
    let receipt = done(release(&w));
    assert_eq!(receipt, UnsentReason::Unrouted.receipt().0);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert_eq!(w.replies.notices().len(), 1);
    assert!(w.replies.outbox().is_empty());
    // Re-registering must never revive a superseded delivery approval.
    w.routes.register(
        "personal",
        "team",
        ConversationKind::Channel,
        "channel-a",
        "channel-a",
    );
    assert_eq!(done(release(&w)), receipt);
    assert!(w.replies.outbox().is_empty());
    assert_eq!(w.replies.notices().len(), 1);
}

#[test]
fn a_recipient_that_leaves_while_the_call_is_being_prepared_is_caught_right_before_the_approval_is_consumed()
 {
    let w = slack_world();
    let nonce = awaiting(create(&w, "c1", &with_destination("report")));
    w.decide(&nonce, OWNER, true).unwrap();
    let routes = w.routes.clone();
    w.ledger.set_after_begin(move || routes.unregister("T1/C1"));
    assert!(matches!(
        create(&w, "c1", &with_destination("report")),
        Err(Error::Denied)
    ));
    assert!(refused_late(&w, "destination_unregistered"));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.schedules.count(), 0);
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    let routes = w.routes.clone();
    w.ledger
        .set_after_begin(move || routes.unregister("channel-a"));
    assert_eq!(done(release(&w)), UnsentReason::Unrouted.receipt().0);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert_eq!(w.replies.notices().len(), 1);
    assert!(w.replies.outbox().is_empty());
}
