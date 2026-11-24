//! 다른 수신자(명시 목적지)를 지정한 예약: 같은 프로필의 호스트 등록부(`ScheduleRoutes`)가 정규 전송 경로로 확정한 값으로만 카드·해시·저장 명세를 만들고,
//! 생성과 매번의 발송에서 소유자가 각각 승인한다. 등록부는 생성·판정·시작·발송 승인 열기·승인 소비 전에 **현재** 상태로 다시 본다(외부 조회는 없다).
//! CLI·HTTP 입구는 원래 대화에만 답하므로 수신자를 지정하면 아무것도 만들지 않는다.

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

/// Slack 같은 대화 입구(1:1)에서 온 소유자의 실행. 등록부에는 `#general`이 정규 경로 `T1/C1`로 등록돼 있다.
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

/// 승인을 소비하기 직전의 재확인이 남기는 거절(호출은 이미 기록돼 있다).
fn refused_late(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("call.refuse".into(), word.into()))
}

fn nothing_recorded(w: &World, call: &str) {
    assert!(!w.ledger.has_call("r1", call), "기록하지 않는다");
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn an_entrance_that_only_answers_in_place_a_missing_registry_or_an_unknown_recipient_creates_nothing()
 {
    // CLI·HTTP 입구는 원래 대화에만 답한다: 등록부에는 묻지도 않는다
    for kind in [ConversationKind::Cli, ConversationKind::Http] {
        let w = slack_world();
        let mut ctx = run_context("personal", "r1", 1, OWNER, "main", &WITH_SCHEDULE);
        ctx.conversation.kind = kind;
        w.ledger.add_run(ctx);
        assert!(matches!(
            create(&w, "c1", &with_destination("보고")),
            Err(Error::Denied)
        ));
        assert!(refused(&w, "destination_unsupported"));
        assert_eq!(w.routes.asked(), 0);
        nothing_recorded(&w, "c1");
    }
    // 등록부를 붙이지 않은 호스트
    let w = slack_world();
    let broker = w.bare().with_schedules(w.schedules.clone());
    assert!(matches!(
        w.call_on(
            &broker,
            "r1",
            1,
            "c1",
            "schedule.create",
            &with_destination("보고")
        ),
        Err(Error::Denied)
    ));
    assert!(refused(&w, "destination_unsupported"));
    nothing_recorded(&w, "c1");
    // 등록부에 없는 수신자
    let w = slack_world();
    let unknown = format!(
        r##"{{"at":"{AT}","instruction":"보고","destination":{{"binding":"slack","conversation":{{"kind":"channel","id":"#random"}}}}}}"##
    );
    assert!(matches!(create(&w, "c1", &unknown), Err(Error::Denied)));
    assert!(refused(&w, "destination_unregistered"));
    assert_eq!(w.routes.asked(), 1);
    nothing_recorded(&w, "c1");
    // 모양이 틀린 수신자는 등록부에 묻기 전에 거절한다
    let bad = format!(
        r#"{{"at":"{AT}","instruction":"보고","destination":{{"binding":"slack","conversation":{{"kind":"cli","id":"x"}}}}}}"#
    );
    assert!(matches!(create(&w, "c2", &bad), Err(Error::Invalid)));
    assert_eq!(w.routes.asked(), 1);
    nothing_recorded(&w, "c2");
}

#[test]
fn a_registered_recipient_is_created_only_after_the_owner_approves_the_canonical_recipient_and_the_instruction()
 {
    // 정책이 승인을 요구하지 않아도 다른 수신자를 지정한 생성은 항상 승인이 필요하다(되돌릴 수 없는 호출)
    let w = slack_world();
    let nonce = awaiting(create(&w, "c1", &with_destination("주간 보고를 보내줘")));
    assert_eq!(w.schedules.count(), 0);
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(stored.approval.tool.0, "schedule.create");
    assert_eq!(stored.approval.tool_revision.0, "builtin-1");
    assert_eq!(stored.approval.risk, Risk::Irreversible);
    assert_eq!(stored.reasons, vec!["irreversible".to_string()]);
    // 카드: 모델이 쓴 이름(#general)이 아니라 등록부가 확정한 정규 경로·스레드와 지시문
    let card = stored.card.0;
    assert!(card.contains("T1/C1") && card.contains("t1"), "{card}");
    assert!(card.contains("주간 보고를 보내줘"), "{card}");
    assert!(!card.contains("#general"), "{card}");
    // 승인하면 한 번만 만들고 저장된 수신자는 정규 경로다
    w.decide(&nonce, OWNER, true).unwrap();
    let receipt = done(create(&w, "c1", &with_destination("주간 보고를 보내줘")));
    assert_eq!(w.schedules.count(), 1);
    let id = receipt.split('"').nth(3).expect("예약 ID").to_owned();
    let stored = w.schedules.destination(&id).expect("저장된 수신자");
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
    // 같은 호출의 재전달은 같은 영수증이고 다시 만들지 않는다
    assert_eq!(
        done(create(&w, "c1", &with_destination("주간 보고를 보내줘"))),
        receipt
    );
    assert_eq!(w.schedules.count(), 1);
    // 원래 대화로 만든 예약에는 수신자가 없다
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
    let nonce = awaiting(create(&w, "c1", &with_destination("보고")));
    assert_eq!(
        w.ledger.stored_approval(&nonce).reasons,
        vec!["irreversible".to_string(), "private_recall".to_string()]
    );
    w.decide(&nonce, OWNER, true).unwrap();
    done(create(&w, "c1", &with_destination("보고")));
    assert_eq!(w.schedules.count(), 1);
}

#[test]
fn the_registry_is_read_again_at_the_decision_and_before_the_creation_approval_is_consumed() {
    // 카드를 띄운 뒤 수신자가 등록부에서 빠졌다: 옛 승인 버튼은 거절된다
    let w = slack_world();
    let nonce = awaiting(create(&w, "c1", &with_destination("보고")));
    w.routes.unregister("T1/C1");
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert_eq!(w.schedules.count(), 0);
    // 승인한 뒤 같은 호출이 다시 들어오기 전에 빠졌다: 승인을 소비하지 않고 만들지 않으며, 돌아오면 같은 승인으로 이어진다
    let w = slack_world();
    let nonce = awaiting(create(&w, "c1", &with_destination("보고")));
    w.decide(&nonce, OWNER, true).unwrap();
    w.routes.unregister("T1/C1");
    assert!(matches!(
        create(&w, "c1", &with_destination("보고")),
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
    done(create(&w, "c1", &with_destination("보고")));
    assert_eq!(w.schedules.count(), 1);
    // 다른 정규 경로로 재지정됐다: 승인한 카드의 수신자와 달라 이어지지 않는다
    let w = slack_world();
    let nonce = awaiting(create(&w, "c1", &with_destination("보고")));
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
        create(&w, "c1", &with_destination("보고")),
        Err(Error::Conflict)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn a_delivery_to_a_recipient_that_left_the_registry_gets_no_card_and_ends_as_unsent() {
    let w = deliver_world();
    w.routes.unregister("channel-a");
    // 수신자가 빠졌다(확정): 카드·기록·발송 없이 이 발생을 «미발송»으로 끝낸다. 원래 대화의 고정 안내 한 건과 미발송 영수증이 결과다
    let receipt = done(release(&w));
    assert_eq!(receipt, UnsentReason::Unrouted.receipt().0);
    assert!(
        !w.ledger.has_call(RUN, DELIVER_CALL),
        "카드를 위한 기록도 만들지 않는다"
    );
    assert_eq!(
        w.replies.notices(),
        vec![(RunId(RUN.into()), UNROUTED_NOTICE.to_owned())]
    );
    assert!(w.replies.outbox().is_empty());
    // 재시도는 같은 영수증이고 안내는 한 건 그대로다
    assert_eq!(done(release(&w)), receipt);
    assert_eq!(w.replies.notices().len(), 1);
    // 등록부를 붙이지 않았거나 일시 오류다: 확인하지 못해 아무것도 하지 않고 같은 발생을 다시 시도한다
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
    // 복구하면 같은 발생이 카드를 연다
    w.routes.outage(false);
    awaiting(release(&w));
}

#[test]
fn a_recipient_that_left_after_the_card_ends_the_occurrence_and_a_return_never_revives_the_old_approval()
 {
    // 카드를 띄운 뒤 빠졌다: 클릭하면 옛 버튼은 낡았고 이 발생은 «미발송»으로 끝난다(안내 한 건·옛 카드 무효·발송 0)
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.routes.unregister("channel-a");
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert_eq!(w.replies.notices().len(), 1);
    assert!(w.replies.outbox().is_empty());
    // 그 뒤 재시도는 같은 영수증이다
    assert_eq!(done(release(&w)), UnsentReason::Unrouted.receipt().0);
    assert_eq!(w.replies.notices().len(), 1);
    // 승인한 뒤 소비하기 전에 빠졌다: 승인을 소비하지 않고 이 발생을 미발송으로 끝낸다
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.routes.unregister("channel-a");
    let receipt = done(release(&w));
    assert_eq!(receipt, UnsentReason::Unrouted.receipt().0);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert_eq!(w.replies.notices().len(), 1);
    assert!(w.replies.outbox().is_empty());
    // 등록부에 돌아와도 그 승인으로 다시 보내지 않는다(발송 단계에서는 등록부에 돌아오면 재개하던 예전 규약을 폐기했다)
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
    // 생성: 등록부를 읽은 뒤(호출 기록 직후) 수신자가 빠졌다
    let w = slack_world();
    let nonce = awaiting(create(&w, "c1", &with_destination("보고")));
    w.decide(&nonce, OWNER, true).unwrap();
    let routes = w.routes.clone();
    w.ledger.set_after_begin(move || routes.unregister("T1/C1"));
    assert!(matches!(
        create(&w, "c1", &with_destination("보고")),
        Err(Error::Denied)
    ));
    assert!(refused_late(&w, "destination_unregistered"));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.schedules.count(), 0);
    // 발송: 같은 틈 — 승인을 소비하지 않고 이 발생을 «미발송»으로 끝낸다
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
