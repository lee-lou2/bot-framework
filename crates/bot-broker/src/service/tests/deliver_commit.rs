//! 예약 회신 전송의 커밋 뒤: 저장소가 같은 커밋에서 run을 끝내 리스를 놓으므로, 응답을 잃은 재시도·정상 커밋 직후는 일반 원장 조회(`Stale`) 대신
//! 전용 영수증 읽기(`ScheduledReplies::receipt`)로 같은 영수증을 돌려준다. 마지막 claim·등록 때의 활성 집합만 읽고, private 조회 이유도 보존한다.

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
    assert_eq!(w.replies.outbox().len(), 1, "커밋은 됐다");
    w.replies.lose_response(false);
    assert_eq!(done(release(&w)), draft(TEXT).queued_receipt().0);
    assert_eq!(w.replies.outbox().len(), 1);
    // 저장소 장애는 보내지도 승인을 소비하지도 않는다
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
    // 저장소가 run을 끝내 리스를 놓았다: 일반 원장 조회는 정상 전송 뒤에도 `Stale`이다
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
    // 그래도 재시도(응답을 잃은 경우)는 전용 읽기로 같은 영수증이다: 초안·권한·승인을 다시 읽지 않고 다시 보내지 않는다
    w.replies.set_draft(RUN, draft("나중에 달라진 내용"));
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
    // 다른 claim·다른 세대·다른 프로필의 시도는 영수증을 읽지 못한다
    assert!(matches!(
        by(&|a| a.claim = Some(ClaimEpoch(99))),
        Err(Error::Stale)
    ));
    assert!(matches!(by(&|a| a.claim = None), Err(Error::Stale)));
    assert!(matches!(
        by(&|a| a.generation = Generation(2)),
        Err(Error::Stale)
    ));
    // 같은 run ID라도 다른 프로필은 등록이 없어 전용 읽기가 `None`이고, 일반 claim 검사가 거절한다
    assert!(matches!(
        by(&|a| a.profile = ProfileId("other".into())),
        Err(Error::Denied)
    ));
    // 최종 claim은 읽는다
    assert_eq!(done(by(&|_| {})), receipt);
    // 등록 때와 다른 활성 집합이면 수신 경로가 달라졌을 수 있어 읽지 못한다
    w.replies.set_active_set("set-b");
    assert!(matches!(by(&|_| {}), Err(Error::Stale)));
    w.replies.set_active_set("set-a");
    assert_eq!(done(by(&|_| {})), receipt);
    // 활성 집합이 없을 때도 이전 영수증이 나오지 않는다
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
    // 등록 전에는 어떤 시도에도 `None`이다(전용 읽기는 실행 권한을 판정하지 않는다)
    assert!(matches!(
        block_on(w.replies.receipt(attempt_of(&w, RUN))),
        Ok(None)
    ));
    // 자격 없는 시도는 일반 claim 검사에서 거절된다
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
    // 등록 전에 영수증 읽기가 막히면(저장소 장애) 보내지도 기록하지도 않는다
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
    // 다른 수신자에게 나가는 호출이라 private 조회가 있었으면 이유가 하나 더 붙는다(카드에도 보인다)
    assert_eq!(
        stored.reasons,
        vec!["irreversible".to_string(), "private_recall".to_string()]
    );
    assert!(
        stored.card.0.contains("이유: irreversible, private_recall"),
        "{}",
        stored.card.0
    );
    // 승인·전송은 같은 이유로 이어진다(결정 때 다시 계산한 이유와 같다)
    w.decide(&nonce, OWNER, true).unwrap();
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
    // private 조회가 없으면 이유는 하나뿐이다
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    assert_eq!(
        w.ledger.stored_approval(&nonce).reasons,
        vec!["irreversible".to_string()]
    );
}
