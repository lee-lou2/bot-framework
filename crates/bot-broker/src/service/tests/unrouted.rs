//! 다른 수신자로 가는 호출의 «수신자 확인» 결과 구분: 확정(같은 경로·빠졌거나 재지정됨)과 확인하지 못함(일시 오류·등록부 미연결),
//! 그리고 소유자의 명시적 거절. 예약 회신 전송에서
//! - 수신자가 빠졌다(확정) → 이 발생을 **미발송(`delivery_unrouted`)**으로 끝낸다: 원래 대화 고정 안내 한 건·옛 카드 무효·미발송 영수증이 저장소의 한 커밋이다.
//! - 소유자가 거절하거나 소비 전에 철회했다 → **거절(`delivery_rejected`)**로 끝낸다(안내 없음).
//! - 확인하지 못했다 → 아무것도 열지·소비하지·닫지 않고 **같은 발생을 다시 시도**한다(`Unavailable`). 발생을 버리지 않는다.
//!
//! 첫 카드는 수신자 확인 뒤 철회돼도 열리지 않는다(카드를 열기 직전에 다시 본다).

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

/// 호출을 기록한 직후 **한 번만** `action`을 한다(훅은 `begin`마다 불리므로 재시도가 다시 걸리지 않게 한 번으로 줄인다).
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
    // 생성: 호출을 기록한 직후(등록부를 처음 본 뒤) 수신자가 빠졌다 → 카드 0, 예약 0
    let w = slack_world();
    let routes = w.routes.clone();
    w.ledger.set_after_begin(move || routes.unregister("T1/C1"));
    assert!(matches!(
        create(&w, "c1", &with_destination("보고")),
        Err(Error::Denied)
    ));
    assert!(w.ledger.pending_approval("r1", "c1").is_none(), "카드 0");
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
    // 전송: 같은 틈 → 카드 0, 이 발생은 미발송으로 끝난다(안내 한 건, outbox 0)
    let w = deliver_world();
    let routes = w.routes.clone();
    w.ledger
        .set_after_begin(move || routes.unregister("channel-a"));
    assert_eq!(done(release(&w)), unrouted());
    assert!(
        w.ledger.pending_approval(RUN, DELIVER_CALL).is_none(),
        "카드 0"
    );
    assert!(w.ledger.approval_ids(RUN).is_empty(), "승인을 열지 않았다");
    assert_eq!(
        w.replies.notices(),
        vec![(RunId(RUN.into()), UNROUTED_NOTICE.to_owned())]
    );
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn an_unconfirmed_registry_retries_the_same_occurrence_at_every_step_and_nothing_is_consumed_or_closed()
 {
    // 시작에서: 기록·카드·안내 없이 재시도
    let w = deliver_world();
    w.routes.outage(true);
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    w.routes.outage(false);
    // 카드를 열기 직전에: 호출은 기록돼 있지만 카드는 없다. 복구하면 같은 호출이 카드를 연다
    let routes = w.routes.clone();
    after_begin_once(&w, move || routes.outage(true));
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert!(w.ledger.approval_ids(RUN).is_empty(), "카드 0");
    assert!(matches!(
        w.ledger.phase(RUN, DELIVER_CALL),
        CallPhase::Open { approval: None }
    ));
    w.routes.outage(false);
    let nonce = awaiting(release(&w));
    // 승인 클릭에서: 버튼을 무효로 하지 않고 이 클릭만 다시 시도한다
    w.routes.outage(true);
    assert!(matches!(
        w.decide(&nonce, OWNER, true),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    w.routes.outage(false);
    w.decide(&nonce, OWNER, true).unwrap();
    // 승인을 소비하기 직전에: 소비하지도 닫지도 않는다. 복구하면 같은 승인으로 한 번 보낸다
    let routes = w.routes.clone();
    after_begin_once(&w, move || routes.outage(true));
    assert!(matches!(release(&w), Err(Error::Unavailable)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert!(w.replies.outbox().is_empty() && w.replies.notices().is_empty());
    w.routes.outage(false);
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
    assert!(w.replies.notices().is_empty());
    // 생성도 같다: 시작·클릭에서 확인하지 못하면 아무것도 만들지 않고 다시 시도한다
    let w = slack_world();
    w.routes.outage(true);
    assert!(matches!(
        create(&w, "c1", &with_destination("보고")),
        Err(Error::Unavailable)
    ));
    assert!(!w.ledger.has_call("r1", "c1"));
    w.routes.outage(false);
    let nonce = awaiting(create(&w, "c1", &with_destination("보고")));
    w.routes.outage(true);
    assert!(matches!(
        w.decide(&nonce, OWNER, true),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    assert_eq!(w.schedules.count(), 0);
    // 승인한 생성을 소비하기 직전에 확인하지 못하면: 소비하지도 만들지도 않고 다시 시도한다. 복구하면 같은 승인으로 한 번 만든다
    w.routes.outage(false);
    w.decide(&nonce, OWNER, true).unwrap();
    let routes = w.routes.clone();
    after_begin_once(&w, move || routes.outage(true));
    assert!(matches!(
        create(&w, "c1", &with_destination("보고")),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.schedules.count(), 0);
    w.routes.outage(false);
    done(create(&w, "c1", &with_destination("보고")));
    assert_eq!(w.schedules.count(), 1);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
}

#[test]
fn a_recipient_re_pointed_to_another_real_destination_ends_as_unrouted_like_a_removed_one() {
    // 같은 바인딩·같은 이름이 지금은 다른 실제 수신자를 가리킨다(등록부가 거절하지 않고 다른 경로를 돌려준다)
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
    // 카드를 띄운 뒤 재지정돼도 같다: 클릭은 낡은 버튼이고 발생은 미발송으로 끝난다
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
    // 소유자가 거절했다
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, false).unwrap();
    assert_eq!(done(release(&w)), rejected());
    assert!(
        w.replies.notices().is_empty(),
        "거절은 안내를 남기지 않는다"
    );
    // 승인했다가 소비 전에 철회했다(소유자의 명시적 결정)
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.decide(&nonce, OWNER, false).unwrap();
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Revoked);
    assert_eq!(done(release(&w)), rejected());
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    // 수신자가 빠졌다: 다른 영수증과 고정 안내 한 건(수신자·초안·지시문 없음)
    let w = deliver_world();
    w.routes.unregister("channel-a");
    assert_eq!(done(release(&w)), unrouted());
    let notices = w.replies.notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].1, UNROUTED_NOTICE);
    assert!(!notices[0].1.contains("channel-a") && !notices[0].1.contains(super::deliver::TEXT));
    assert_ne!(unrouted(), rejected());
    // 두 결과는 서로 바꿔 쓰지 못한다: 끝난 발생의 다른 이유 요청은 충돌이다
    let again = block_on(w.replies.finish_unsent(
        DeliveryFence::Claim(super::deliver::attempt_of(&w, RUN)),
        super::deliver::draft(super::deliver::TEXT).target,
        UnsentReason::Rejected,
    ));
    assert!(matches!(again, Err(Error::Conflict)));
}

#[test]
fn after_a_new_activation_a_removed_recipient_is_told_apart_from_a_stale_draft() {
    // 활성 집합이 바뀌어 초안은 옛 집합에서 동결한 것이 됐다. 그 사이 수신자가 빠졌다 → «보낼 수 없는 초안(Conflict)»에 가려지지 않고 미발송으로 끝난다
    let w = deliver_world();
    w.replies.set_active_set("set-b");
    w.routes.unregister("channel-a");
    assert_eq!(done(release(&w)), unrouted());
    assert_eq!(w.replies.notices().len(), 1);
    assert!(w.replies.outbox().is_empty());
    // 수신자는 그대로다 → 예전처럼 본문을 내주지 않고 `Conflict`(옛 집합의 초안은 보내지 않는다)
    let w = deliver_world();
    w.replies.set_active_set("set-b");
    assert!(matches!(release(&w), Err(Error::Conflict)));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    // 확인하지 못한다 → 다시 시도
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
    // 결정만 받는 브로커에 회신 저장소를 붙이지 않았다: 미발송 종결을 못 하므로 낡은 버튼으로 닫지 않고 다시 시도하게 한다
    let decider = w.bare().with_routes(w.routes.clone());
    let result = block_on(decider.decide(nonce.clone(), PrincipalId(OWNER.into()), true));
    assert!(matches!(result, Err(Error::Unavailable)), "{result:?}");
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    assert!(w.replies.notices().is_empty());
    // 저장소가 붙은 브로커는 그 클릭으로 끝낸다
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
