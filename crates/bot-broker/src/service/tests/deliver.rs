//! Scheduled reply delivery (`schedule.deliver`) via host-only `ScheduleDelivery::release`.

use super::{
    schedule::{OWNER, WITH_SCHEDULE, at_args, schedule_world, snapshot_with},
    *,
};
use crate::canon::sha256;
use bot_core::{
    cards::LABEL_TOOL,
    command::JsonPayload,
    ledger::{ApprovalState, CallLedger, CallPhase, StartCall},
    schedule::{
        DELIVER_CALL, DELIVER_REVISION, DELIVER_TOOL, DELIVERY_CARD_LABEL, DeliveryDraft,
        DeliveryTarget, ScheduleDelivery,
    },
    state::{Conversation, ConversationKind, Risk},
    trace::key,
};

pub(super) const RUN: &str = "r1";
pub(super) const TEXT: &str = "Sending monthly report";

fn draft_of(answers: &[&str]) -> DeliveryDraft {
    DeliveryDraft {
        activation: Revision("set-a".into()),
        target: DeliveryTarget {
            binding: BindingId("team".into()),
            conversation: Conversation {
                kind: ConversationKind::Channel,
                id: "channel-a".into(),
                thread: None,
            },
        },
        answers: answers.iter().map(|a| JsonPayload((*a).into())).collect(),
    }
}

fn text_answer(text: &str) -> String {
    serde_json::json!({ "text": text }).to_string()
}

pub(super) fn draft(text: &str) -> DeliveryDraft {
    draft_of(&[&text_answer(text)])
}

pub(super) fn deliver_world() -> World {
    let w = schedule_world();
    w.replies.set_draft(RUN, draft(TEXT));
    w
}

pub(super) fn attempt_of(w: &World, run: &str) -> Attempt {
    Attempt {
        profile: ProfileId("personal".into()),
        run: RunId(run.into()),
        claim: Some(w.ledger.claim_epoch(run)),
        generation: Generation(1),
    }
}

pub(super) fn release_on(
    w: &World,
    broker: &BrokerService,
    run: &str,
) -> Result<CallResult, Error> {
    block_on(broker.release(attempt_of(w, run)))
}

pub(super) fn release(w: &World) -> Result<CallResult, Error> {
    release_on(w, &w.broker, RUN)
}

fn refused(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("schedule.refuse".into(), word.into()))
}

fn card_document(w: &World, nonce: &ApprovalId) -> String {
    let card = w.ledger.stored_approval(nonce).card.0;
    let value: serde_json::Value = serde_json::from_str(&card).expect("card JSON");
    let lines: Vec<&str> = value["approval"]["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| line.as_str().expect("line"))
        .collect();
    let label = format!("{DELIVERY_CARD_LABEL}: ");
    let first = lines
        .iter()
        .position(|line| line.starts_with(&label))
        .expect("delivery draft line");
    let mut out = lines[first][label.len()..].to_owned();
    for line in &lines[first + 1..] {
        match line.strip_prefix("  ") {
            Some(rest) => out.push_str(rest),
            None => break,
        }
    }
    out
}

#[test]
fn the_owner_sees_the_whole_draft_and_it_is_sent_once_after_approval() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    assert!(w.replies.outbox().is_empty());
    assert!(matches!(
        w.ledger.phase(RUN, DELIVER_CALL),
        CallPhase::Open { approval: Some(_) }
    ));
    let stored = w.ledger.stored_approval(&nonce);
    let expected = draft(TEXT);
    assert_eq!(stored.approval.tool.0, DELIVER_TOOL);
    assert_eq!(stored.approval.tool_revision.0, DELIVER_REVISION);
    assert_eq!(stored.approval.call.0, DELIVER_CALL);
    assert_eq!(stored.approval.risk, Risk::Irreversible);
    assert_eq!(
        (
            stored.approval.requester.0.as_str(),
            stored.approval.approver.0.as_str()
        ),
        (OWNER, OWNER)
    );
    assert_eq!(stored.approval.args_sha256, expected.sha256().unwrap());
    assert_eq!(stored.reasons, vec!["irreversible".to_string()]);
    let document = card_document(&w, &nonce);
    assert_eq!(document, expected.approval_json().unwrap());
    assert_eq!(sha256(&document), stored.approval.args_sha256);
    assert!(document.contains(TEXT) && document.contains("channel-a") && document.contains("team"));
    assert!(
        stored
            .card
            .0
            .contains(&format!("{LABEL_TOOL}: schedule.deliver")),
        "{}",
        stored.card.0
    );
    w.decide(&nonce, OWNER, true).unwrap();
    let receipt = done(release(&w));
    assert_eq!(receipt, r#"{"queued":1}"#);
    assert_eq!(receipt, expected.queued_receipt().0);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    let sent = w.replies.outbox();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].binding.as_str(), sent[0].conversation.as_str()),
        ("team", "channel-a")
    );
    assert_eq!(sent[0].answers, vec![text_answer(TEXT)]);
    // Retries replay the stored receipt without re-reading the draft or sending again.
    assert_eq!(done(release(&w)), receipt);
    w.replies.set_draft(RUN, draft("changed content later"));
    assert_eq!(done(release(&w)), receipt);
    assert_eq!(w.replies.outbox().len(), 1);
    let spans = w.tracer.spans();
    assert!(spans.iter().all(|s| {
        s.str(key::GEN_AI_OPERATION_NAME) == Some("execute_tool")
            && s.str(key::GEN_AI_TOOL_TYPE) == Some("function")
            && s.str(key::GEN_AI_TOOL_NAME) == Some(DELIVER_TOOL)
            && s.attrs.iter().all(|(k, _)| key::ALLOWED.contains(k))
    }));
    assert_eq!(spans[0].str(key::BOT_CALL_STATE), Some("awaiting"));
    assert_eq!(spans[0].str(key::BOT_TOOL_RISK), Some("irreversible"));
    assert!(format!("{spans:?}").find(TEXT).is_none());
}

#[test]
fn a_pending_card_is_the_same_card_until_it_is_decided() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    assert_eq!(awaiting(release(&w)), nonce, "must not open a second card");
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn every_answer_and_attachment_is_on_the_card_in_order_or_nothing_is_sent() {
    let w = schedule_world();
    let first = text_answer("first reply");
    let second = serde_json::json!({
        "text": "next",
        "attachments": [{"name": "note.txt", "type": "text/plain", "data": "b25lCnR3bw=="}]
    })
    .to_string();
    w.replies.set_draft(RUN, draft_of(&[&first, &second]));
    let nonce = awaiting(release(&w));
    let document = card_document(&w, &nonce);
    let (a, b) = (
        document.find("first reply").unwrap(),
        document.find("next").unwrap(),
    );
    assert!(a < b, "answers must appear in send order: {document}");
    assert!(
        document.contains(r#""text":"one\ntwo""#),
        "decoded attachment text must be visible: {document}"
    );
    w.decide(&nonce, OWNER, true).unwrap();
    assert_eq!(done(release(&w)), r#"{"queued":2}"#);
    assert_eq!(w.replies.outbox()[0].answers, vec![first, second]);
}

#[test]
fn a_draft_the_card_cannot_show_in_full_is_refused_without_any_record() {
    let image = serde_json::json!({
        "text": "image",
        "attachments": [{"name": "a.png", "type": "image/png", "data": "aGk="}]
    })
    .to_string();
    for answers in [
        vec![image],
        vec![text_answer("ok"), r#"{"text":"x","extra":1}"#.to_owned()],
        vec![r#"{"attachments":[]}"#.to_owned()],
        vec![],
    ] {
        let w = schedule_world();
        let refs: Vec<&str> = answers.iter().map(String::as_str).collect();
        w.replies.set_draft(RUN, draft_of(&refs));
        assert!(matches!(release(&w), Err(Error::Denied)), "{answers:?}");
        assert!(refused(&w, "card_unsupported"));
        assert!(
            !w.ledger.has_call(RUN, DELIVER_CALL),
            "must not record call"
        );
        assert!(w.replies.outbox().is_empty());
    }
}

#[test]
fn the_card_line_limit_is_checked_before_recording_and_the_edge_fits_exactly() {
    // Fits within core's document limit (2500 * 3 bytes) but exceeds the card's 20-line limit once headers are counted.
    let w = schedule_world();
    w.replies.set_draft(RUN, draft(&"★".repeat(2500)));
    assert!(draft(&"★".repeat(2500)).approval_json().is_ok());
    assert!(matches!(release(&w), Err(Error::Denied)));
    assert!(refused(&w, "card_unsupported"));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    let fits = |n: usize| {
        let w = schedule_world();
        w.replies.set_draft(RUN, draft(&"x".repeat(n)));
        match release(&w) {
            Ok(CallResult::Awaiting(nonce)) => {
                assert_eq!(
                    card_document(&w, &nonce),
                    draft(&"x".repeat(n)).approval_json().unwrap(),
                    "all {n} bytes must appear in full"
                );
                true
            }
            Err(Error::Denied) => false,
            other => panic!("{n}: {other:?}"),
        }
    };
    let (mut low, mut high) = (1usize, 9000usize);
    assert!(fits(low) && !fits(high));
    while high - low > 1 {
        let mid = (low + high) / 2;
        if fits(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    assert!(fits(low) && !fits(low + 1), "boundary {low}");
}

#[test]
fn without_a_frozen_draft_or_a_store_nothing_happens() {
    let w = schedule_world();
    assert!(matches!(release(&w), Err(Error::Missing)));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    let w = deliver_world();
    let bare = w.bare();
    assert!(matches!(
        release_on(&w, &bare, RUN),
        Err(Error::Unavailable)
    ));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn only_the_human_who_made_the_schedule_can_release_it() {
    let w = deliver_world();
    w.ledger.add_run(run_context(
        "personal",
        "r2",
        1,
        "service:morning",
        "main",
        &WITH_SCHEDULE,
    ));
    w.replies.set_draft("r2", draft(TEXT));
    assert!(matches!(
        release_on(&w, &w.broker, "r2"),
        Err(Error::Denied)
    ));
    assert!(refused(&w, "not_human"));
    let mut ctx = run_context("personal", "r3", 1, OWNER, "main", &WITH_SCHEDULE);
    ctx.conversation.kind = ConversationKind::Scheduled;
    w.ledger.add_run(ctx);
    w.replies.set_draft("r3", draft(TEXT));
    assert!(matches!(
        release_on(&w, &w.broker, "r3"),
        Err(Error::Denied)
    ));
    assert!(refused(&w, "not_owner"));
    // Trigger whose approver differs from requester has no requester-as-owner approver here.
    w.publish(
        snapshot_with("r1b")
            .cron_trigger("main", "alice-job", "local:alice", Some(OWNER))
            .build(),
    );
    let mut ctx = run_context("personal", "r4", 1, "local:alice", "main", &WITH_SCHEDULE);
    ctx.conversation.kind = ConversationKind::Scheduled;
    ctx.trigger = Some(bot_core::state::TriggerRef {
        agent: AgentId("main".into()),
        id: "alice-job".into(),
    });
    w.ledger.add_run(ctx);
    w.replies.set_draft("r4", draft(TEXT));
    assert!(matches!(
        release_on(&w, &w.broker, "r4"),
        Err(Error::Denied)
    ));
    assert!(!w.ledger.has_call("r4", DELIVER_CALL));
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(release(&w), Err(Error::Denied)));
    assert!(refused(&w, "scope_missing"));
    for run in ["r1", "r2", "r3", "r4"] {
        assert!(
            !w.ledger.has_call(run, DELIVER_CALL),
            "{run}: must not record call"
        );
    }
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn only_the_designated_human_decides_and_a_stale_button_changes_nothing() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    assert!(matches!(
        w.decide(&nonce, "local:alice", true),
        Err(Error::Denied)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    w.publish(snapshot_with("r2").build());
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert!(w.replies.outbox().is_empty());
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn an_owner_rejection_ends_the_occurrence_as_rejected_and_an_expired_approval_is_only_denied() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, false).unwrap();
    let receipt = done(release(&w));
    assert_eq!(
        receipt,
        bot_core::schedule::UnsentReason::Rejected.receipt().0
    );
    assert!(w.replies.outbox().is_empty());
    assert!(w.replies.notices().is_empty());
    assert!(matches!(
        w.ledger.phase(RUN, DELIVER_CALL),
        CallPhase::Done(_)
    ));
    assert_eq!(done(release(&w)), receipt);
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.clock.advance(Duration::from_secs(31 * 60));
    assert!(matches!(release(&w), Err(Error::Denied)));
    assert!(w.replies.outbox().is_empty());
    assert!(w.replies.notices().is_empty());
    assert_ne!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
}

#[test]
fn a_draft_or_recipient_that_changed_after_the_card_is_never_sent_and_keeps_the_approval() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.replies
        .set_draft(RUN, draft("completely different content"));
    assert!(matches!(release(&w), Err(Error::Conflict)));
    assert!(w.replies.outbox().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    w.replies.set_draft(RUN, draft(TEXT));
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
    // Draft modified between begin and commit: store recomputes digest and rejects.
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    let replies = w.replies.clone();
    w.ledger
        .set_after_begin(move || replies.set_draft(RUN, draft("changed right before commit")));
    assert!(matches!(release(&w), Err(Error::Conflict)));
    assert!(w.replies.outbox().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    // Active activation set changed after the card was issued.
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.replies.set_active_set("set-b");
    assert!(matches!(release(&w), Err(Error::Conflict)));
    assert!(w.replies.outbox().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
}

#[test]
fn the_general_path_can_never_preclaim_the_reserved_call_id() {
    let w = deliver_world();
    let create = at_args("x");
    for (tool, args) in [
        ("agenda.list", "{}"),
        ("schedule.deliver", "{}"),
        ("schedule.create", create.as_str()),
        ("schedule.list", "{}"),
        ("memory.share", r#"{"id":"m1"}"#),
        ("not.a.tool", "{}"),
    ] {
        assert!(
            matches!(w.call(DELIVER_CALL, tool, args), Err(Error::Denied)),
            "{tool}"
        );
    }
    assert!(
        !w.ledger.has_call(RUN, DELIVER_CALL),
        "must not record under any tool"
    );
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.refuse".into(), "reserved_call".into()))
    );
    // Calling the host-only tool under another call ID returns Missing.
    assert!(matches!(
        w.call("c1", "schedule.deliver", "{}"),
        Err(Error::Missing)
    ));
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
}

#[test]
fn the_generic_start_path_cannot_run_a_delivery_call() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    let start = StartCall {
        checked_revision: Revision("r1".into()),
        risk: Risk::Irreversible,
        tool_revision: Revision(DELIVER_REVISION.into()),
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId(RUN.into()),
            claim: Some(w.ledger.claim_epoch(RUN)),
            generation: Generation(1),
        },
        call: CallId(DELIVER_CALL.into()),
        approval: None,
        private_recall: false,
        owner: "x".into(),
    };
    assert!(matches!(
        block_on(w.ledger.start(start)),
        Err(Error::Denied)
    ));
    assert!(w.replies.outbox().is_empty());
}
