//! 기록된 소유자 결정(거절·소비 전 철회)은 그 뒤의 등록부 상태보다 우선한다: 수신자가 빠졌든 확인하지 못하든 이 발생은 **거절(`delivery_rejected`)**로 끝나고
//! 원래 대화 안내는 남지 않는다. 확인하지 못한다고 같은 발생을 영원히 다시 시도하지도, 빠졌다고 미발송으로 덮지도 않는다.

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

/// 소유자가 이 카드를 어떻게 결정했는가.
#[derive(Clone, Copy, Debug)]
enum Verdict {
    /// 승인 전에 거절했다(`Denied`).
    Denied,
    /// 승인했다가 소비 전에 철회했다(`Revoked`).
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

/// 등록부가 소유자 결정 뒤에 어떻게 됐는가.
#[derive(Clone, Copy, Debug)]
enum Registry {
    /// 수신자가 빠졌다(확정).
    Gone,
    /// 확인하지 못한다(일시 오류).
    Transient,
    /// 그대로 같은 경로다(결정만 있다).
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
/// 소비 직전 철회 경합은 등록부가 멀쩡한 경우도 본다(소비하려는 순간 승인이 이미 철회돼 있다).
const RACE_REGISTRIES: [Registry; 3] = [Registry::Gone, Registry::Transient, Registry::Same];

fn assert_rejected_quietly(w: &World, nonce: &ApprovalId, state: ApprovalState, what: &str) {
    assert!(
        w.replies.notices().is_empty(),
        "{what}: 거절은 안내를 남기지 않는다"
    );
    assert!(w.replies.outbox().is_empty(), "{what}: 보낸 것이 없다");
    assert_eq!(w.ledger.approval_state(nonce), state, "{what}");
}

#[test]
fn a_recorded_owner_decision_wins_over_a_removed_or_unreachable_recipient() {
    for verdict in VERDICTS {
        for registry in REGISTRIES {
            let what = format!("{verdict:?} × {registry:?}");
            let w = deliver_world();
            let nonce = awaiting(release(&w));
            verdict.record(&w, &nonce);
            registry.apply(&w);
            let state = w.ledger.approval_state(&nonce);
            assert_eq!(done(release(&w)), rejected(), "{what}");
            assert_rejected_quietly(&w, &nonce, state, &what);
            // 재전달은 같은 영수증이다(끝난 발생은 등록부를 다시 보지 않는다)
            assert_eq!(done(release(&w)), rejected(), "{what}: 재전달");
            assert!(
                w.replies.notices().is_empty(),
                "{what}: 재전달 뒤에도 안내 0"
            );
        }
    }
}

#[test]
fn an_owner_decision_also_wins_when_the_draft_was_frozen_under_an_older_set() {
    // 활성 집합이 바뀌어 초안이 옛 집합의 것이 됐다(`Conflict`). 소유자가 이미 거절했다면 «보낼 수 없는 초안»도 «빠진 수신자»도 아니다: 거절이다
    for verdict in VERDICTS {
        for registry in [None, Some(Registry::Gone), Some(Registry::Transient)] {
            let what = format!("{verdict:?} × {registry:?} × 새 집합");
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
    // 결정이 없으면(대기 중·승인됨) 예전 그대로다: 빠졌으면 미발송, 확인하지 못하면 다시 시도
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
    // 게이트가 승인(Approved)을 읽은 뒤, 소비 직전 등록부 확인에서 수신자가 빠졌거나 확인되지 않는데 그 사이 소유자가 철회했다 → 거절이다
    for registry in RACE_REGISTRIES {
        let what = format!("철회 × {registry:?}");
        let w = deliver_world();
        let nonce = awaiting(release(&w));
        w.decide(&nonce, OWNER, true).unwrap();
        let (routes, ledger, owner) = (w.routes.clone(), w.ledger.clone(), nonce.clone());
        // 이번 `release`의 등록부 확인: ① 초안 직후 ② 소비 직전. ②에서 철회와 등록부 변화가 함께 일어난다
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
    // 브로커가 «빠졌다»고 알고 끝내려는 사이 소유자 결정이 저장됐다면 저장소가 같은 트랜잭션에서 거절로 가른다(영수증이 결과, 안내 0)
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
        assert!(w.replies.notices().is_empty(), "{verdict:?}: 안내 0");
        // 어느 이유로 다시 요청해도 같은 영수증이다(재전달: 결정이 저장돼 있으니 «미발송» 요청도 같은 거절로 가려진다)
        for reason in [UnsentReason::Unrouted, UnsentReason::Rejected] {
            let again = block_on(w.replies.finish_unsent(
                DeliveryFence::Claim(super::deliver::attempt_of(&w, RUN)),
                super::deliver::draft(super::deliver::TEXT).target,
                reason,
            ))
            .unwrap();
            assert_eq!(again.0, rejected(), "{verdict:?} 재전달 {reason:?}");
        }
        assert!(w.replies.notices().is_empty());
        assert!(w.ledger.has_call(RUN, DELIVER_CALL));
    }
}

#[test]
fn a_click_that_races_with_a_stored_owner_decision_is_a_stale_button_and_the_resumed_attempt_rejects()
 {
    // 승인 클릭이 «수신자가 빠졌다»로 발생을 끝내려는 사이 같은 소유자의 거절이 먼저 저장됐다: 그 클릭은 낡은 버튼이고(안내 0) 발생은 이어 받은 시도가 거절로 끝낸다
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
    // «거절»은 소유자가 저장한 결정이 있을 때만 쓴다: 카드가 없거나 대기·승인 중이면 거절로 끝낼 수 없다
    let reject = |w: &World| {
        block_on(w.replies.finish_unsent(
            DeliveryFence::Claim(super::deliver::attempt_of(w, RUN)),
            super::deliver::draft(super::deliver::TEXT).target,
            UnsentReason::Rejected,
        ))
    };
    let w = deliver_world();
    assert!(matches!(reject(&w), Err(Error::Denied)), "카드 없음");
    let nonce = awaiting(release(&w));
    assert!(matches!(reject(&w), Err(Error::Denied)), "대기 중");
    w.decide(&nonce, OWNER, true).unwrap();
    assert!(matches!(reject(&w), Err(Error::Denied)), "승인됨");
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    // 아무것도 바뀌지 않았다: 같은 승인으로 한 번 보낼 수 있다
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
}

#[test]
fn a_rejected_consumption_with_a_stored_revocation_is_finished_as_rejected_and_other_denials_stay_denials()
 {
    // 소비를 시도하는 순간 승인이 철회돼 있어 저장소가 `Denied`로 거절했다: 소유자 결정이 먼저이니 «거절»로 끝낸다(재호출도 같은 영수증, 안내 0)
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
    assert_eq!(done(release(&w)), rejected(), "재호출도 같은 영수증");
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Revoked);
    // 활성 집합이 함께 바뀌어 `Conflict`로 거절돼도 저장된 철회가 먼저다
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
    // 소유자 결정이 없는 `Denied`는 그대로 `Denied`다(다른 이유의 거절을 «거절»로 덮지 않는다)
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.replies.fail_with(Some(Error::Denied));
    assert!(matches!(release(&w), Err(Error::Denied)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert!(w.replies.notices().is_empty() && w.replies.outbox().is_empty());
}
