//! 결과를 돌려주기 직전의 확인: 저장소가 같은 시점에서 보는 활성 판본·현재 claim, 그리고 기록된 도구·인자 해시.

use super::*;
use bot_core::ledger::CallPhase;

#[test]
fn a_revision_published_by_another_process_hides_the_result_even_when_this_catalog_is_stale() {
    let w = World::new();
    let ledger = w.ledger.clone();
    // 실행을 마치고 완료를 적는 사이에 다른 프로세스가 새 판본을 게시했다(이 프로세스의 카탈로그는 아직 r1)
    w.ledger
        .set_after_finish(move || ledger.set_active_revision("personal", "r2"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
    assert!(
        matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)),
        "결과는 원장에 보존한다"
    );
    // 같은 호출 ID의 재전달도 저장소의 활성 판본이 바뀐 동안은 저장된 결과를 내보내지 않고 다시 실행하지도 않는다
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
    // 판본이 다시 같아지면 같은 결과를 본다
    w.ledger.set_active_revision("personal", "r1");
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
}

#[test]
fn a_result_is_not_returned_to_an_attempt_that_lost_its_claim_while_the_tool_ran() {
    let w = World::new();
    let ledger = w.ledger.clone();
    w.ledger.set_after_finish(move || {
        ledger.bump_claim("r1");
    });
    let result = w.call("c1", "agenda.list", "{}");
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)));
    // 새 claim이 같은 호출 ID로 다시 들어오면 기록된 결과를 받는다(다시 실행하지 않는다)
    let again = w.call("c1", "agenda.list", "{}");
    assert_eq!(done(again), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn a_profile_without_an_active_revision_in_the_store_gets_no_result() {
    let w = World::new();
    let ledger = w.ledger.clone();
    w.ledger
        .set_after_finish(move || ledger.set_active_revision("personal", ""));
    // 활성 판본이 비어 있다고 적힌 경우는 `r1`과 다르므로 충돌이다. 프로필 자체가 활성에서 빠졌으면 거절이다.
    assert!(w.call("c1", "agenda.list", "{}").is_err());
    let w = World::new();
    w.ledger.set_active_revision("other-profile", "x");
    let ledger = w.ledger.clone();
    w.ledger
        .set_before_start(move || ledger.set_active_revision("personal", "r1"));
    assert!(w.call("c1", "agenda.list", "{}").is_ok());
}

#[test]
fn a_cached_result_needs_the_same_tool_and_canonical_arguments_even_if_the_store_does_not_check() {
    let w = World::new();
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
    // 저장소가 해시·도구가 다른 기록도 그대로 돌려주는 경우에도 브로커가 걸러 낸다
    w.ledger.set_lenient_begin(true);
    assert!(matches!(
        w.call("c1", "agenda.list", "{\"extra\":1}"),
        Err(Error::Conflict) | Err(Error::Invalid)
    ));
    assert!(matches!(
        w.call("c1", "agenda.update", "{}"),
        Err(Error::Conflict) | Err(Error::Invalid) | Err(Error::Denied)
    ));
    assert_eq!(w.runner.jobs().len(), 1, "다시 실행하지 않는다");
}
