//! 원격(Streamable HTTP) 서버로 가는 호출의 브로커 쪽 규칙: 정의가 고정과 다르면 호출 전에 거절하고 고정 코드를 감사에 남기며,
//! 호스트가 서버에 묶은 자격(bearer)이 보관소에 없으면 승인 카드도 열지 않고, 복구에서 «연결을 닫았다»를 «원격 실행이 끝났다»의 증명으로 쓰지 않는다.
//! 위험 등급별 승인은 로컬 도구와 같은 경로다(아래 첫 시험이 같은 승인 한 건이 쓰이는 것을 본다).
use super::*;
use crate::ports::Ran;
use bot_core::ledger::{ApprovalState, CallPhase};

const DELETE: &str = "{\"event\":\"walk\"}";

fn approve(w: &World, call: &str) -> ApprovalId {
    let nonce = awaiting(w.call(call, "agenda.delete", DELETE));
    w.decide(&nonce, "local:owner", true).unwrap();
    nonce
}

fn audited(w: &World, code: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("call.refuse".into(), code.into()))
}

#[test]
fn a_definition_mismatch_after_approval_is_refused_with_a_fixed_code_and_closes_the_call_for_good()
{
    for code in ["tool_definition_changed", "tool_missing"] {
        let w = World::new();
        let nonce = approve(&w, "c1");
        w.runner.script(Ran::Refused(code));
        let result = w.call("c1", "agenda.delete", DELETE);
        assert!(matches!(result, Err(Error::Denied)), "{code}: {result:?}");
        assert_eq!(w.runner.jobs().len(), 1, "{code}: 호출은 한 번 시도했다");
        assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Denied));
        assert!(audited(&w, code), "{code}: {:?}", w.audit.verdicts());
        // 쓴 승인은 되살아나지 않고, 같은 호출 ID를 다시 보내도 새 실행도 새 승인도 없다
        assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
        let again = w.call("c1", "agenda.delete", DELETE);
        assert!(matches!(again, Err(Error::Denied)), "{code}: {again:?}");
        assert_eq!(w.runner.jobs().len(), 1, "{code}: 자동 재실행 0건");
    }
}

#[test]
fn a_refused_read_is_denied_too_and_nothing_is_retried() {
    let w = World::new();
    w.runner.script(Ran::Refused("tool_definition_changed"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(audited(&w, "tool_definition_changed"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn a_server_bound_credential_the_store_lacks_refuses_a_new_call_before_any_card_or_run() {
    let w = World::new();
    w.runner.bind_server_secret("demo-agenda", "docs-token");
    w.runner.miss_secret("docs-token");
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(audited(&w, "secret_missing"));
    assert!(!w.ledger.has_call("r1", "c1"), "기록하지 않는다");
    assert!(w.ledger.audit_log().is_empty(), "승인 카드도 없다");
    assert!(w.runner.jobs().is_empty());
    // 그 프로필의 이름공간에서 서버에 묶은 이름을 물었다
    assert_eq!(
        w.runner.secret_checks(),
        vec![(ProfileId("personal".into()), vec!["docs-token".to_owned()])]
    );
    // 보관소를 채우면 같은 호출 ID가 승인 대기로 이어진다
    w.runner.restore_secret("docs-token");
    awaiting(w.call("c1", "agenda.delete", DELETE));
    assert!(w.ledger.has_call("r1", "c1"));
}

#[test]
fn a_credential_bound_to_another_server_is_not_asked_for() {
    let w = World::new();
    w.runner.bind_server_secret("other-server", "docs-token");
    w.runner.miss_secret("docs-token");
    awaiting(w.call("c1", "agenda.delete", DELETE));
    assert!(
        w.runner.secret_checks().is_empty(),
        "이 서버에 묶인 자격도 선언한 자격도 없으니 보관소를 읽지 않는다"
    );
}

#[test]
fn closing_the_connection_is_not_proof_that_a_remote_call_ended() {
    // 원격 서버: 읽기 호출도 옛 호출이 끝났다는 증명이 없으면 다시 실행하지 않고 보류한다
    let w = World::new();
    w.runner.reap_proves_nothing_for("demo-agenda");
    w.leave_started("r1", "c1", "agenda.list", "{}", 1);
    w.ledger.bump_generation("r1");
    assert!(is_held(&w.call_on(
        &w.broker,
        "r1",
        2,
        "c1",
        "agenda.list",
        "{}"
    )));
    assert_eq!(w.runner.reaped(), vec!["r1.1.c1".to_string()]);
    assert!(w.runner.jobs().is_empty(), "다시 실행하지 않는다");
    match w.ledger.phase("r1", "c1") {
        CallPhase::Held { reason, .. } => assert_eq!(reason, "previous_process_unconfirmed"),
        other => panic!("{other:?}"),
    }
    // 대조: 로컬 서버(증명이 되는 정리)는 같은 상황에서 읽기를 다시 실행한다
    let local = World::new();
    local.leave_started("r1", "c1", "agenda.list", "{}", 1);
    local.ledger.bump_generation("r1");
    done(local.call_on(&local.broker, "r1", 2, "c1", "agenda.list", "{}"));
    assert_eq!(local.runner.jobs().len(), 1);
}

#[test]
fn an_irreversible_remote_write_waits_for_the_same_single_approval_as_a_local_one() {
    let w = World::new();
    w.runner.bind_server_secret("demo-agenda", "docs-token");
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty(), "승인 전 실행 0건");
    assert_eq!(
        w.ledger.stored_approval(&nonce).reasons,
        vec!["irreversible".to_string()]
    );
    w.decide(&nonce, "local:owner", true).unwrap();
    done(w.call("c1", "agenda.delete", DELETE));
    assert_eq!(w.runner.jobs().len(), 1, "승인 한 건으로 한 번만 실행한다");
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
}
