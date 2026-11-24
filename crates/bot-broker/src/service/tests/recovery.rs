use super::*;
use crate::ports::Ran;
use bot_core::{
    ledger::{CallPhase, Resolution},
    state::EffectState,
};

const DELETE: &str = "{\"event\":\"walk\"}";

fn approve_and_start(w: &World, call: &str) {
    let nonce = awaiting(w.call(call, "agenda.delete", DELETE));
    w.decide(&nonce, "local:owner", true).unwrap();
}

#[test]
fn an_unknown_write_result_is_held_and_never_retried_by_itself() {
    let w = World::new();
    approve_and_start(&w, "c1");
    w.runner.script(Ran::Unknown("response_unknown"));
    assert!(is_held(&w.call("c1", "agenda.delete", DELETE)));
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Held { .. }));
    assert!(is_held(&w.call("c1", "agenda.delete", DELETE)));
    // 워커가 바뀌어도 판정이 없으면 계속 보류다
    w.ledger.bump_generation("r1");
    assert!(is_held(&w.call_on(
        &w.broker,
        "r1",
        2,
        "c1",
        "agenda.delete",
        DELETE
    )));
    assert_eq!(w.runner.jobs().len(), 1, "자동 재실행 0건");
}

#[test]
fn after_an_admin_verdict_the_same_call_id_resumes_with_a_fresh_approval() {
    let w = World::new();
    approve_and_start(&w, "c1");
    w.runner.script(Ran::Unknown("response_unknown"));
    assert!(is_held(&w.call("c1", "agenda.delete", DELETE)));
    w.ledger.bump_generation("r1");
    w.ledger.resolve("r1", "c1", Resolution::NotExecuted);
    // 현재 권한·새 세대의 새 승인으로 같은 호출 ID를 재개한다
    let fresh = awaiting(w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE));
    assert_eq!(w.runner.jobs().len(), 1, "새 승인 전 재실행 0건");
    w.decide(&fresh, "local:owner", true).unwrap();
    done(w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE));
    let jobs = w.runner.jobs();
    assert_eq!(jobs.len(), 2);
    assert_eq!(
        (jobs[1].call.0.as_str(), jobs[1].attempt.generation.0),
        ("c1", 2)
    );
    assert_eq!(jobs[1].owner, "r1.2.c1");
}

#[test]
fn a_started_call_left_by_a_lost_worker_waits_until_the_old_process_is_confirmed_gone() {
    let w = World::new();
    w.leave_started("r1", "c1", "agenda.delete", DELETE, 1);
    w.ledger.bump_generation("r1");
    w.runner.set_reap_ok(false); // 호스트 단절 등으로 종료를 확인하지 못한다
    assert!(is_held(&w.call_on(
        &w.broker,
        "r1",
        2,
        "c1",
        "agenda.delete",
        DELETE
    )));
    assert_eq!(w.runner.reaped(), vec!["r1.1.c1".to_string()]);
    assert!(w.runner.jobs().is_empty());
    match w.ledger.phase("r1", "c1") {
        CallPhase::Held { reason, .. } => assert_eq!(reason, "previous_process_unconfirmed"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_single_write_without_evidence_stays_held_even_after_the_old_process_is_gone() {
    let w = World::new();
    w.leave_started("r1", "c1", "agenda.delete", DELETE, 1);
    w.ledger.bump_generation("r1");
    assert!(is_held(&w.call_on(
        &w.broker,
        "r1",
        2,
        "c1",
        "agenda.delete",
        DELETE
    )));
    match w.ledger.phase("r1", "c1") {
        CallPhase::Held {
            reason, resolution, ..
        } => {
            assert_eq!(reason, "response_unknown");
            assert!(resolution.is_none());
        }
        other => panic!("{other:?}"),
    }
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_read_left_started_resumes_once_the_old_process_is_gone() {
    let w = World::new();
    w.leave_started("r1", "c1", "agenda.list", "{}", 1);
    w.ledger.bump_generation("r1");
    assert_eq!(
        done(w.call_on(&w.broker, "r1", 2, "c1", "agenda.list", "{}")),
        "{\"ok\":true}"
    );
    assert_eq!(w.runner.reaped(), vec!["r1.1.c1".to_string()]);
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(w.runner.jobs()[0].owner, "r1.2.c1");
}

#[test]
fn an_admin_receipt_completes_the_call_without_running_it() {
    let w = World::new();
    w.leave_started("r1", "c1", "agenda.delete", DELETE, 1);
    w.ledger.bump_generation("r1");
    assert!(is_held(&w.call_on(
        &w.broker,
        "r1",
        2,
        "c1",
        "agenda.delete",
        DELETE
    )));
    w.ledger.resolve(
        "r1",
        "c1",
        Resolution::Executed {
            receipt: "evt-9".into(),
        },
    );
    assert_eq!(
        done(w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE)),
        "{\"receipt\":\"evt-9\"}"
    );
    assert!(
        w.runner.jobs().is_empty(),
        "영수증이 있으면 실행하지 않는다"
    );
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)));
}

#[test]
fn a_multi_step_tool_resumes_only_when_no_step_is_unresolved() {
    let w = World::new();
    w.leave_started("r1", "c1", "agenda.sync", "{\"n\":1}", 1);
    w.effects
        .set_unresolved(&[("e-1", EffectState::Held, Some("response_unknown"))]);
    w.ledger.bump_generation("r1");
    assert!(is_held(&w.call_on(
        &w.broker,
        "r1",
        2,
        "c1",
        "agenda.sync",
        "{\"n\":1}"
    )));
    match w.ledger.phase("r1", "c1") {
        CallPhase::Held { reason, .. } => assert_eq!(reason, "effects_unresolved"),
        other => panic!("{other:?}"),
    }
    // 미결이 없으면(모두 확정되거나 시작 전) 같은 호출 ID로 이어 간다
    w.effects.set_unresolved(&[]);
    w.leave_started("r1", "c2", "agenda.sync", "{\"n\":2}", 2);
    w.ledger.bump_generation("r1");
    done(w.call_on(&w.broker, "r1", 3, "c2", "agenda.sync", "{\"n\":2}"));
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(w.runner.jobs()[0].call.0, "c2");
}

#[test]
fn a_tool_that_answers_with_unresolved_steps_is_not_completed() {
    let w = World::new();
    w.effects
        .set_unresolved(&[("e-1", EffectState::Started, None)]);
    assert!(is_held(&w.call("c1", "agenda.sync", "{}")));
    match w.ledger.phase("r1", "c1") {
        CallPhase::Held { reason, .. } => assert_eq!(reason, "effects_unresolved"),
        other => panic!("{other:?}"),
    }
    // 단계 원장을 조회하지 못해도 확인된 것이 아니다
    let w = World::new();
    w.effects.set_unavailable(true);
    assert!(is_held(&w.call("c1", "agenda.sync", "{}")));
}

#[test]
fn an_unknown_read_result_releases_the_call_for_a_retry() {
    let w = World::new();
    w.runner.script(Ran::Unknown("timeout"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Unavailable)
    ));
    assert!(matches!(
        w.ledger.phase("r1", "c1"),
        CallPhase::Open { approval: None }
    ));
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 2);
}

#[test]
fn a_failure_before_sending_releases_the_call_and_a_retry_needs_a_fresh_approval() {
    let w = World::new();
    let first = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.decide(&first, "local:owner", true).unwrap();
    w.runner.script(Ran::NotSent);
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Unavailable)
    ));
    assert!(matches!(
        w.ledger.phase("r1", "c1"),
        CallPhase::Open { approval: None }
    ));
    let second = awaiting(w.call("c1", "agenda.delete", DELETE));
    assert_ne!(second.0, first.0, "소비한 승인은 다시 쓰지 않는다");
    w.decide(&second, "local:owner", true).unwrap();
    done(w.call("c1", "agenda.delete", DELETE));
    assert_eq!(w.runner.jobs().len(), 2);
}

#[test]
fn a_runner_that_fails_after_start_holds_a_write_and_releases_a_read() {
    let w = World::new();
    approve_and_start(&w, "c1");
    w.runner.fail_next();
    assert!(is_held(&w.call("c1", "agenda.delete", DELETE)));
    w.runner.fail_next();
    assert!(matches!(
        w.call("c2", "agenda.list", "{}"),
        Err(Error::Unavailable)
    ));
}

#[test]
fn a_blocked_result_is_final_and_recorded() {
    let w = World::new();
    w.runner.script(Ran::Blocked("result_credential"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Denied));
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.blocked".into(), "result_credential".into()))
    );
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn the_same_generation_cannot_start_the_same_call_twice() {
    let w = World::new();
    w.leave_started("r1", "c1", "agenda.list", "{}", 1);
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_blocked_write_result_is_held_because_the_effect_is_unknown() {
    let w = World::new();
    approve_and_start(&w, "c1");
    w.runner.script(Ran::Blocked("result_credential"));
    assert!(
        is_held(&w.call("c1", "agenda.delete", DELETE)),
        "쓰기는 도구가 이미 실행됐다"
    );
    match w.ledger.phase("r1", "c1") {
        CallPhase::Held { reason, .. } => assert_eq!(reason, "result_credential"),
        other => panic!("{other:?}"),
    }
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.blocked".into(), "result_credential".into()))
    );
    assert!(is_held(&w.call("c1", "agenda.delete", DELETE)));
    assert_eq!(w.runner.jobs().len(), 1, "다시 실행하지 않는다");
}
