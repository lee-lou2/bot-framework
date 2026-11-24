//! 현재 권한: 저장된 결과·승인자·판본·claim은 판정 시점이 아니라 실행 시점의 값으로 다시 본다.

use super::*;
use bot_core::{
    id::ClaimEpoch,
    ledger::{ApprovalState, CallLedger, CallPhase, Resolution},
    state::{ConversationKind, TriggerRef},
};

const DELETE: &str = "{\"event\":\"walk\"}";

fn without(scope: &str) -> Vec<&str> {
    ALL.iter().copied().filter(|s| *s != scope).collect()
}

fn scheduled(w: &World, run: &str, approver: &str) {
    let mut ctx = run_context(
        "personal",
        run,
        1,
        "service:morning",
        "main",
        &["calendar.delete"],
    );
    ctx.approver = Some(PrincipalId(approver.into()));
    ctx.conversation.kind = ConversationKind::Scheduled;
    ctx.trigger = Some(TriggerRef {
        agent: AgentId("main".into()),
        id: "morning".into(),
    });
    w.ledger.add_run(ctx);
}

#[test]
fn a_stored_result_is_not_returned_after_the_role_is_revoked() {
    let w = World::new();
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
    w.publish(base("r2").role("owner", &without("calendar.read")).build());
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert_eq!(w.runner.jobs().len(), 1, "철회 뒤 원격 재실행 0건");
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.replay".into(), "scope_missing".into()))
    );
    assert!(
        matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)),
        "결과는 돌려주지 않을 뿐 기록은 그대로"
    );
    // 권한이 돌아오면 같은 결과를 본다
    w.publish(base("r3").build());
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
}

#[test]
fn a_recovery_receipt_is_not_returned_after_the_permission_is_revoked() {
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
    w.publish(
        base("r2")
            .role("owner", &without("calendar.delete"))
            .build(),
    );
    assert!(matches!(
        w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(
        matches!(
            w.ledger.phase("r1", "c1"),
            CallPhase::Held {
                resolution: Some(_),
                ..
            }
        ),
        "보류 증거는 지우지 않는다"
    );
    assert!(w.runner.jobs().is_empty());
    w.publish(base("r3").build());
    assert_eq!(
        done(w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE)),
        "{\"receipt\":\"evt-9\"}"
    );
}

#[test]
fn a_call_left_started_stays_held_for_evidence_when_the_permission_is_revoked() {
    let w = World::new();
    w.leave_started("r1", "c1", "agenda.delete", DELETE, 1);
    w.ledger.bump_generation("r1");
    w.publish(
        base("r2")
            .role("owner", &without("calendar.delete"))
            .build(),
    );
    assert!(matches!(
        w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(
        w.runner.reaped().is_empty(),
        "권한이 없으면 복구 판정도 하지 않는다"
    );
    assert!(matches!(
        w.ledger.phase("r1", "c1"),
        CallPhase::Started { .. }
    ));
}

#[test]
fn approval_cards_follow_the_current_approver_not_the_one_recorded_at_acceptance() {
    let w = World::new();
    scheduled(&w, "r2", "local:alice");
    let old = awaiting(w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE));
    assert_eq!(
        w.ledger.stored_approval(&old).approval.approver.0,
        "local:alice"
    );
    // 새 판본이 승인자를 owner로 바꾼다
    w.publish(
        base("r2")
            .clear_triggers("main")
            .cron_trigger("main", "morning", "service:morning", Some("local:owner"))
            .build(),
    );
    assert!(
        matches!(w.decide(&old, "local:alice", true), Err(Error::Stale)),
        "옛 승인자의 버튼은 거절"
    );
    assert_eq!(w.ledger.approval_state(&old), ApprovalState::Superseded);
    let new = awaiting(w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE));
    assert_eq!(
        w.ledger.stored_approval(&new).approval.approver.0,
        "local:owner",
        "새 카드는 새 승인자에게 간다"
    );
    assert!(matches!(
        w.decide(&new, "local:alice", true),
        Err(Error::Denied)
    ));
    w.decide(&new, "local:owner", true).unwrap();
    done(w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn nothing_that_needs_approval_runs_when_the_current_approver_cannot_be_confirmed() {
    let w = World::new();
    scheduled(&w, "r2", "local:alice");
    // 트리거가 승인자를 잃었다 / 승인자가 멤버에서 빠졌다
    w.publish(
        base("r2")
            .clear_triggers("main")
            .cron_trigger("main", "morning", "service:morning", None)
            .build(),
    );
    assert!(matches!(
        w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    let without_alice = base("r3")
        .clear_triggers("main")
        .cron_trigger("main", "morning", "service:morning", Some("local:ghost"))
        .build();
    w.publish(without_alice);
    assert!(matches!(
        w.call_on(&w.broker, "r2", 1, "c2", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(
        w.audit
            .verdicts()
            .iter()
            .filter(|(_, v)| v == "no_approver")
            .count()
            == 2
    );
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_requester_removed_from_the_profile_cannot_be_asked_or_run_anything() {
    let w = World::new();
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.publish(
        snapshot("personal", "r2")
            .role("owner", &ALL)
            .policy_scopes(&ALL)
            .agent("main")
            .build(),
    );
    assert!(
        matches!(w.decide(&nonce, "local:owner", true), Err(Error::Stale)),
        "멤버가 아니면 승인할 수 없다"
    );
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_revocation_between_judgement_and_start_never_reaches_the_runner() {
    let w = World::new();
    let (catalog, lowered) = (
        w.catalog.clone(),
        base("r2").role("owner", &without("calendar.read")).build(),
    );
    w.ledger
        .set_after_begin(move || catalog.publish(lowered.clone()));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert!(
        w.runner.jobs().is_empty(),
        "begin과 start 사이 철회 시 실행기 호출 0건"
    );
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Open { .. }));
    // 같은 호출 ID로 다시 들어오면 새 판본으로 다시 판정한다
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn the_agent_ceiling_of_the_pinned_revision_still_applies_when_the_current_one_is_wider() {
    let w = World::new();
    let narrow_then: Vec<&str> = without("calendar.delete");
    w.publish(base("r0").agent_scopes("main", &narrow_then).build());
    w.ledger.add_run({
        let mut ctx = owner_run("r9", 1);
        ctx.work.revision = Revision("r0".into());
        ctx
    });
    w.publish(base("r1b").build()); // 지금 판본은 넓다
    assert!(matches!(
        w.call_on(&w.broker, "r9", 1, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    // 지금 판본이 좁으면 고정 판본이 넓어도 거절한다
    w.publish(base("r2").agent_scopes("main", &narrow_then).build());
    assert!(matches!(
        w.call("c2", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn an_approval_survives_a_normal_reclaim_but_a_stale_claim_cannot_operate() {
    let w = World::new();
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    // 승인 대기 중 워커가 리스를 놓았다가 같은 세대에서 새 리스를 잡았다
    let old_claim = w.ledger.claim_epoch("r1");
    let new_claim = w.ledger.bump_claim("r1");
    assert_ne!(old_claim, new_claim);
    w.decide(&nonce, "local:owner", true).unwrap(); // 승인은 세대에 묶인다
    done(w.call("c1", "agenda.delete", DELETE)); // 새 claim으로 소비한다
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    // 옛 claim을 쥔 워커는 같은 세대여도 아무것도 하지 못한다
    let stale = |claim: Option<ClaimEpoch>| Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        generation: Generation(1),
        claim,
    };
    assert!(matches!(
        block_on(w.ledger.context(stale(Some(old_claim)))),
        Err(Error::Stale)
    ));
    assert!(matches!(
        block_on(w.ledger.context(stale(None))),
        Err(Error::Denied)
    ));
    let call = ToolCall {
        attempt: stale(Some(old_claim)),
        call: CallId("c2".into()),
        tool: ToolId("agenda.list".into()),
        args: JsonPayload("{}".into()),
    };
    assert!(matches!(
        block_on(bot_core::port::Broker::call(&w.broker, call)),
        Err(Error::Stale)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn the_approval_context_is_actor_bound_and_identity_only() {
    let w = World::new();
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    let who = |p: &str| PrincipalId(p.into());
    assert!(matches!(
        block_on(
            w.ledger
                .approval_context(nonce.clone(), who("local:mallory"))
        ),
        Err(Error::Denied)
    ));
    let ctx = block_on(w.ledger.approval_context(nonce.clone(), who("local:owner"))).unwrap();
    assert!(ctx.attempt.claim.is_none(), "신원만 돌려준다");
    assert!(
        matches!(
            block_on(w.ledger.begin(
                ctx.attempt.clone(),
                CallId("c9".into()),
                ToolId("agenda.list".into()),
                [0; 32]
            )),
            Err(Error::Denied)
        ),
        "워커 연산을 인가하지 못한다"
    );
    assert_eq!(ctx.principal.0, "local:owner");
    w.ledger.bump_generation("r1");
    assert!(matches!(
        block_on(w.ledger.approval_context(nonce, who("local:owner"))),
        Err(Error::Stale)
    ));
}

#[test]
fn the_stored_approval_never_keeps_a_claim() {
    let w = World::new();
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    assert!(
        w.ledger
            .stored_approval(&nonce)
            .approval
            .attempt
            .claim
            .is_none()
    );
}

#[test]
fn a_secret_the_profile_does_not_list_is_never_resolved() {
    let w = World::new();
    let mut needy = tool(Risk::Read, &["calendar.read"]);
    needy
        .secrets
        .insert("TOKEN".into(), SecretName::parse("reader").unwrap());
    // 도구는 reader를 선언했지만 이 프로필의 secrets 목록에는 없다
    w.publish(
        base("r0")
            .tool("main", "agenda.list", needy.clone())
            .build(),
    );
    w.ledger.add_run({
        let mut ctx = owner_run("r9", 1);
        ctx.work.revision = Revision("r0".into());
        ctx
    });
    assert!(matches!(
        w.call_on(&w.broker, "r9", 1, "c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.deny".into(), "secret_not_allowed".into()))
    );
    assert!(w.runner.jobs().is_empty());
    // 프로필이 허용한 이름이면 실행기까지 간다
    w.publish(
        base("r0")
            .secret("reader")
            .tool("main", "agenda.list", needy)
            .build(),
    );
    done(w.call_on(&w.broker, "r9", 1, "c2", "agenda.list", "{}"));
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn a_stored_result_is_withheld_when_the_revision_changes_while_it_is_read() {
    let w = World::new();
    done(w.call("c1", "agenda.list", "{}"));
    let (catalog, lowered) = (
        w.catalog.clone(),
        base("r2").role("owner", &without("calendar.read")).build(),
    );
    w.ledger
        .set_after_begin(move || catalog.publish(lowered.clone()));
    // 판정에 쓴 판본(r1)이 결과를 돌려주기 직전에 이미 철회돼 있다
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
    // 다시 들어오면 새 판본으로 판정한다
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
}

#[test]
fn the_store_closes_the_gap_between_the_last_recheck_and_the_start_commit() {
    let w = World::new();
    let ledger = w.ledger.clone();
    // 브로커가 판본을 다시 확인한 뒤, 원장이 start를 받기 직전에 새 판본이 활성이 된다
    w.ledger
        .set_before_start(move || ledger.set_active_revision("personal", "r2"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Stale)
    ));
    assert!(
        w.runner.jobs().is_empty(),
        "start 커밋이 거절되면 실행기는 불리지 않는다"
    );
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Open { .. }));
}

#[test]
fn removing_only_the_origin_triggers_approver_does_not_move_the_card_to_a_sibling_trigger() {
    let w = World::new();
    scheduled(&w, "r2", "local:alice");
    // 같은 서비스 주체의 예약이 둘이고, 승인자는 한쪽(morning)만 지운다
    w.publish(
        base("r2")
            .clear_triggers("main")
            .cron_trigger("main", "morning", "service:morning", None)
            .cron_trigger("main", "evening", "service:morning", Some("local:owner"))
            .build(),
    );
    assert!(matches!(
        w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.deny".into(), "no_approver".into()))
    );
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_result_is_kept_in_the_ledger_but_not_returned_when_the_revision_changes_while_it_is_written() {
    let w = World::new();
    let (catalog, lowered) = (
        w.catalog.clone(),
        base("r2").role("owner", &without("calendar.read")).build(),
    );
    w.ledger
        .set_after_finish(move || catalog.publish(lowered.clone()));
    // 완료를 기록하는 동안 철회됐다: 호출은 한 번 실행됐고 원장에는 Done이 있지만 본문은 돌려주지 않는다
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)));
    // 같은 호출 ID로 다시 들어오면 새 판본의 권한을 먼저 확인하므로 저장된 결과도 나가지 않는다
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert_eq!(w.runner.jobs().len(), 1, "다시 실행하지 않는다");
}

#[test]
fn a_recovery_receipt_is_kept_but_not_returned_when_the_revision_changes_while_it_is_written() {
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
    let (catalog, lowered) = (
        w.catalog.clone(),
        base("r2")
            .role("owner", &without("calendar.delete"))
            .build(),
    );
    w.ledger
        .set_after_finish(move || catalog.publish(lowered.clone()));
    assert!(matches!(
        w.call_on(&w.broker, "r1", 2, "c1", "agenda.delete", DELETE),
        Err(Error::Conflict)
    ));
    assert!(
        matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)),
        "영수증은 원장에 보존했다"
    );
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_profile_that_is_no_longer_active_makes_the_store_refuse_to_start() {
    let w = World::new();
    w.ledger.set_active_revision("other-profile", "x"); // personal의 활성 판본 기록이 없는 저장소
    let ledger = w.ledger.clone();
    w.ledger
        .set_before_start(move || ledger.set_active_revision("personal", "gone"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Stale)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_tool_can_never_receive_the_hosts_private_intake_key_even_if_the_profile_lists_it() {
    let w = World::new();
    let mut greedy = tool(Risk::Read, &["calendar.read"]);
    greedy
        .secrets
        .insert("KEY".into(), SecretName::parse("private-intake").unwrap());
    // 프로필의 허용 목록에 이름이 있어도 호스트 전용이라 풀지 않는다
    w.publish(
        base("r1")
            .secret("private-intake")
            .tool("main", "agenda.list", greedy)
            .build(),
    );
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.deny".into(), "secret_reserved".into()))
    );
    assert!(w.runner.jobs().is_empty());
}
