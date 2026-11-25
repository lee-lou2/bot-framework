//! 기준 구현(`MemLedger`)이 지키는 불변식. 실제 저장소가 같은 시험을 통과해야 한다.

use super::*;
use bot_core::ledger::*;

/// 시험 장부는 세대가 오를 때마다 claim epoch도 하나 오른다(처음 1·1, 인계하면 2·2).
fn attempt(generation: u64) -> Attempt {
    Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        claim: Some(bot_core::id::ClaimEpoch(generation)),
        generation: Generation(generation),
    }
}

fn ledger() -> (Arc<MemLedger>, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(1_000));
    let ledger = Arc::new(MemLedger::new(clock.clone()));
    ledger.add_run(owner_run("r1", 1));
    ledger.set_active_revision("personal", "r1");
    (ledger, clock)
}

fn begin(ledger: &MemLedger, call: &str) {
    block_on(ledger.begin(
        attempt(1),
        CallId(call.into()),
        ToolId("web.search".into()),
        [1; 32],
    ))
    .unwrap();
}

fn start(call: &str, private_recall: bool) -> StartCall {
    StartCall {
        checked_revision: Revision("r1".into()),
        risk: bot_core::state::Risk::Read,
        tool_revision: Revision("test".into()),
        attempt: attempt(1),
        call: CallId(call.into()),
        approval: None,
        private_recall,
        owner: "r1.1.c".into(),
    }
}

#[test]
fn a_private_flag_raised_between_judgement_and_start_is_a_conflict() {
    let (ledger, _) = ledger();
    begin(&ledger, "c1");
    ledger.set_private_recall("r1"); // 브로커가 플래그를 읽은 뒤 다른 실행이 private 기억을 조회했다
    assert!(matches!(
        block_on(ledger.start(start("c1", false))),
        Err(Error::Conflict)
    ));
    assert!(matches!(ledger.phase("r1", "c1"), CallPhase::Open { .. }));
    // 플래그를 알고 판정했다면(승인 대상으로) 시작할 수 있다
    block_on(ledger.start(start("c1", true))).unwrap();
}

#[test]
fn every_write_is_fenced_to_the_current_generation() {
    let (ledger, _) = ledger();
    begin(&ledger, "c1");
    ledger.bump_generation("r1");
    assert!(matches!(
        block_on(ledger.context(attempt(1))),
        Err(Error::Stale)
    ));
    assert!(matches!(
        block_on(ledger.begin(
            attempt(1),
            CallId("c2".into()),
            ToolId("web.search".into()),
            [1; 32]
        )),
        Err(Error::Stale)
    ));
    assert!(matches!(
        block_on(ledger.start(start("c1", false))),
        Err(Error::Stale)
    ));
    assert!(matches!(
        block_on(ledger.finish(FinishCall {
            attempt: attempt(1),
            call: CallId("c1".into()),
            outcome: Outcome::Blocked
        })),
        Err(Error::Stale)
    ));
}

#[test]
fn a_cancelled_run_cannot_start_anything() {
    let (ledger, _) = ledger();
    begin(&ledger, "c1");
    ledger.cancel("r1");
    assert!(matches!(
        block_on(ledger.start(start("c1", false))),
        Err(Error::Stale)
    ));
}

#[test]
fn begin_is_idempotent_and_refuses_a_different_tool_or_hash() {
    let (ledger, _) = ledger();
    begin(&ledger, "c1");
    begin(&ledger, "c1");
    let other_tool = block_on(ledger.begin(
        attempt(1),
        CallId("c1".into()),
        ToolId("a.b".into()),
        [1; 32],
    ));
    let other_hash = block_on(ledger.begin(
        attempt(1),
        CallId("c1".into()),
        ToolId("web.search".into()),
        [2; 32],
    ));
    assert!(
        matches!(other_tool, Err(Error::Conflict)) && matches!(other_hash, Err(Error::Conflict))
    );
}

#[test]
fn a_started_call_cannot_start_again_in_the_same_or_a_later_generation_without_release() {
    let (ledger, _) = ledger();
    begin(&ledger, "c1");
    block_on(ledger.start(start("c1", false))).unwrap();
    assert!(matches!(
        block_on(ledger.start(start("c1", false))),
        Err(Error::Conflict)
    ));
    ledger.bump_generation("r1");
    let later = StartCall {
        attempt: attempt(2),
        ..start("c1", false)
    };
    assert!(matches!(
        block_on(ledger.start(later)),
        Err(Error::Conflict)
    ));
    // 미실행이 입증돼 풀리면 다시 시작할 수 있다
    block_on(ledger.finish(FinishCall {
        attempt: attempt(2),
        call: CallId("c1".into()),
        outcome: Outcome::Released,
    }))
    .unwrap();
    let again = StartCall {
        attempt: attempt(2),
        ..start("c1", false)
    };
    block_on(ledger.start(again)).unwrap();
}

#[test]
fn opening_a_new_approval_supersedes_the_old_one_and_decisions_wake_the_run() {
    let (ledger, _) = ledger();
    begin(&ledger, "c1");
    let open = |nonce: &str| OpenApproval {
        approval: bot_core::state::Approval {
            nonce: ApprovalId(nonce.into()),
            attempt: attempt(1),
            call: CallId("c1".into()),
            requester: PrincipalId("local:owner".into()),
            approver: PrincipalId("local:owner".into()),
            risk: bot_core::state::Risk::Read,
            policy_revision: Revision("p".into()),
            tool: ToolId("web.search".into()),
            tool_revision: Revision("t".into()),
            args_sha256: [1; 32],
            expires_at: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(5_000),
        },
        reasons: vec![],
        card: JsonPayload("{}".into()),
        delivery: None,
    };
    block_on(ledger.open_approval(open("n1"))).unwrap();
    block_on(ledger.open_approval(open("n2"))).unwrap();
    assert_eq!(
        ledger.approval_state(&ApprovalId("n1".into())),
        ApprovalState::Superseded
    );
    assert_eq!(ledger.pending_approval("r1", "c1").unwrap().0, "n2");
    assert!(ledger.woken().is_empty());
    let decision = |allow| Decision {
        nonce: ApprovalId("n2".into()),
        actor: PrincipalId("local:owner".into()),
        allow,
    };
    assert_eq!(
        block_on(ledger.decide(decision(true))).unwrap(),
        ApprovalState::Approved
    );
    assert_eq!(ledger.woken().len(), 1);
    assert!(matches!(
        block_on(ledger.decide(decision(true))),
        Err(Error::Conflict)
    ));
}
