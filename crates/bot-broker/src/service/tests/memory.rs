//! 기억 공유(`memory.share`): 소유자가 본 본문 그대로를 승인에 묶고, 한 번만 공유한다.

use super::*;
use crate::{canon::sha256, service::share::share_class};
use bot_core::{
    ledger::{ApprovalState, CallLedger, StartCall},
    state::{ConversationKind, Visibility},
};

const WITH_SHARE: [&str; 8] = [
    "run",
    "calendar.read",
    "calendar.delete",
    "calendar.write",
    "web.search",
    "mail.send",
    "chat.post",
    "memory.share",
];
const NOTE: &str = "The garage code is 4711.\nKeep it in the safe.";
const OWNER: &str = "local:owner";

fn share_snapshot(revision: &str) -> SnapshotBuilder {
    base(revision)
        .role("owner", &WITH_SHARE)
        .policy_scopes(&WITH_SHARE)
}

/// 소유자 본인의 CLI 실행(`r1`)과 소유자의 private 기억 `m1`이 있는 세계. 모든 스코프가 열려 있다.
fn share_world() -> World {
    let w = World::new();
    w.publish(share_snapshot("r1").build());
    w.ledger
        .add_run(run_context("personal", "r1", 1, OWNER, "main", &WITH_SHARE));
    w.memory.add("m1", OWNER, Visibility::Private, NOTE);
    w
}

fn share(w: &World, call: &str, id: &str) -> Result<CallResult, Error> {
    w.call(call, "memory.share", &format!(r#"{{"id":"{id}"}}"#))
}

fn memory_id(id: &str) -> MemoryId {
    MemoryId(id.into())
}

fn refused(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("memory.share.refuse".into(), word.into()))
}

#[test]
fn the_owner_approves_the_exact_text_and_it_is_shared_once() {
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    assert_eq!(w.memory.previews().len(), 1, "ID 한 건을 한 번만 읽는다");
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(
        stored.approval.args_sha256,
        memory_share_hash(&memory_id("m1"), &sha256(NOTE)),
        "승인 해시는 카드에 보인 본문과 기억 ID에서 나온다"
    );
    assert_eq!(stored.approval.tool.0, "memory.share");
    assert_eq!(stored.approval.tool_revision.0, "builtin-1");
    assert_eq!(stored.approval.risk, Risk::Irreversible);
    assert_eq!(stored.approval.requester.0, OWNER);
    assert_eq!(stored.approval.approver.0, OWNER);
    assert_eq!(stored.reasons, share_class().codes());
    // 카드에는 본문 전체와 기억 ID가 있다(자르거나 요약하지 않는다)
    let card = &stored.card.0;
    assert!(
        card.contains("The garage code is 4711.") && card.contains("Keep it in the safe."),
        "{card}"
    );
    assert!(card.contains("m1"));
    assert!(w.memory.shared().is_empty());
    assert_eq!(w.memory.visibility("m1"), Visibility::Private);
    // 승인을 기다리는 동안 같은 호출을 다시 보내도 같은 승인을 가리킨다
    assert_eq!(awaiting(share(&w, "c1", "m1")), nonce);
    w.decide(&nonce, OWNER, true).unwrap();
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    assert_eq!(w.memory.shared(), [memory_id("m1")]);
    assert_eq!(w.memory.visibility("m1"), Visibility::Profile);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    assert!(w.runner.jobs().is_empty(), "도구 서버를 거치지 않는다");
}

#[test]
fn a_denied_card_shares_nothing() {
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.decide(&nonce, OWNER, false).unwrap();
    assert!(matches!(share(&w, "c1", "m1"), Err(Error::Denied)));
    assert_eq!(w.memory.visibility("m1"), Visibility::Private);
    assert!(w.memory.shared().is_empty());
    // 요청자가 아닌 사람이 대신 승인할 수는 없다
    let other = awaiting(share(&w, "c2", "m1"));
    assert!(matches!(
        w.decide(&other, "local:alice", true),
        Err(Error::Denied)
    ));
    assert_eq!(w.ledger.approval_state(&other), ApprovalState::Pending);
}

#[test]
fn a_replayed_completed_call_gets_its_receipt_even_though_the_memory_is_shared_now() {
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.decide(&nonce, OWNER, true).unwrap();
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    assert_eq!(w.memory.visibility("m1"), Visibility::Profile);
    // 같은 호출 ID의 재전달: 영수증이 그대로 돌아오고 다시 공유하지 않는다
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    assert_eq!(w.memory.shared().len(), 1);
    // 재시작한 브로커도 같다
    let restarted = w.reboot();
    let replay = w.call_on(&restarted, "r1", 1, "c1", "memory.share", r#"{"id":"m1"}"#);
    assert_eq!(done(replay), r#"{"id":"m1"}"#);
    assert_eq!(w.memory.shared().len(), 1);
    // 이미 공유된 기억을 새 호출로 또 공유하려는 것은 충돌이고, 승인도 열지 않는다
    assert!(matches!(share(&w, "c2", "m1"), Err(Error::Conflict)));
    assert!(w.ledger.pending_approval("r1", "c2").is_none());
}

#[test]
fn a_body_that_changes_after_the_card_is_never_shared_on_the_old_approval() {
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.decide(&nonce, OWNER, true).unwrap();
    // 읽은 직후·공유 직전에 본문이 바뀐다: 저장소가 지금 본문으로 지문을 다시 계산해 거절한다
    w.memory
        .edit_after_next_preview("m1", "Edited after the card was shown.");
    assert!(matches!(share(&w, "c1", "m1"), Err(Error::Conflict)));
    assert_eq!(
        w.ledger.approval_state(&nonce),
        ApprovalState::Approved,
        "승인은 소비되지 않는다"
    );
    assert_eq!(w.memory.visibility("m1"), Visibility::Private);
    assert!(w.memory.shared().is_empty());
    // 호출 사이에 고친 본문으로는 같은 호출 ID를 다시 열 수 없다
    assert!(matches!(share(&w, "c1", "m1"), Err(Error::Conflict)));
    // 새 호출은 새 본문을 보여 주는 새 승인을 연다
    let fresh = awaiting(share(&w, "c2", "m1"));
    assert_ne!(fresh, nonce);
    assert!(
        w.ledger
            .stored_approval(&fresh)
            .card
            .0
            .contains("Edited after the card was shown.")
    );
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert!(w.memory.shared().is_empty());
}

#[test]
fn only_the_owner_in_their_own_direct_or_cli_conversation_starts_a_share() {
    let w = share_world();
    let mut channel = run_context("personal", "r1", 1, OWNER, "main", &WITH_SHARE);
    channel.conversation.kind = ConversationKind::Channel;
    w.ledger.add_run(channel);
    assert!(matches!(share(&w, "c1", "m1"), Err(Error::Denied)));
    assert!(refused(&w, "not_owner"));
    assert!(
        w.memory.previews().is_empty(),
        "채널에서는 본문을 읽지도 않는다"
    );
    // 1:1 DM이면 된다
    let mut direct = run_context("personal", "r2", 1, OWNER, "main", &WITH_SHARE);
    direct.conversation.kind = ConversationKind::Direct;
    w.ledger.add_run(direct);
    awaiting(w.call_on(&w.broker, "r2", 1, "c2", "memory.share", r#"{"id":"m1"}"#));
    // 서비스 주체(예약 실행)는 사람이 아니라 승인자가 없다
    let service = run_context("personal", "r3", 1, "service:morning", "main", &WITH_SHARE);
    w.ledger.add_run(service);
    let result = w.call_on(&w.broker, "r3", 1, "c3", "memory.share", r#"{"id":"m1"}"#);
    assert!(matches!(result, Err(Error::Denied)));
    assert!(w.memory.shared().is_empty());
}

#[test]
fn another_owners_or_a_missing_memory_is_refused_without_a_record() {
    let w = share_world();
    w.memory
        .add("m2", "local:alice", Visibility::Private, "alice's note");
    assert!(matches!(share(&w, "c1", "m2"), Err(Error::Denied)));
    assert!(matches!(share(&w, "c2", "nope"), Err(Error::Denied)));
    assert!(refused(&w, "memory_unavailable"));
    assert!(w.memory.previews().is_empty());
    assert!(!w.ledger.has_call("r1", "c1") && !w.ledger.has_call("r1", "c2"));
}

#[test]
fn the_scope_must_be_held_by_the_requester_the_agent_and_the_run() {
    // 역할이 지금 판본에서 스코프를 주지 않는다
    let w = share_world();
    w.publish(base("r2").build());
    assert!(matches!(share(&w, "c1", "m1"), Err(Error::Denied)));
    assert!(refused(&w, "scope_missing"));
    // 에이전트 상한에 없다
    let w = share_world();
    w.publish(share_snapshot("r1").agent_scopes("main", &ALL).build());
    assert!(matches!(share(&w, "c1", "m1"), Err(Error::Denied)));
    // 접수 때 고정한 요청자 권한에 없다
    let w = share_world();
    w.ledger.add_run(owner_run("r1", 1));
    assert!(matches!(share(&w, "c1", "m1"), Err(Error::Denied)));
    assert!(
        w.memory.previews().is_empty(),
        "스코프가 없으면 본문을 읽지 않는다"
    );
    assert!(w.memory.shared().is_empty());
}

#[test]
fn malformed_arguments_are_refused_before_the_memory_is_read() {
    let w = share_world();
    for (i, args) in [
        "{}",
        r#"{"id":""}"#,
        r#"{"id":"a b"}"#,
        r#"{"id":5}"#,
        r#"{"id":"m1","extra":1}"#,
        r#"{"id":"m1","id":"m1"}"#,
        "[]",
    ]
    .iter()
    .enumerate()
    {
        let result = w.call(&format!("bad{i}"), "memory.share", args);
        assert!(matches!(result, Err(Error::Invalid)), "{args}");
    }
    assert!(w.memory.previews().is_empty());
}

#[test]
fn a_body_the_card_cannot_show_in_full_is_not_shared() {
    let w = share_world();
    w.memory
        .add("big", OWNER, Visibility::Private, &"x".repeat(8_000));
    assert!(matches!(share(&w, "c1", "big"), Err(Error::Denied)));
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.deny".into(), "card_too_large".into()))
    );
    assert!(w.ledger.pending_approval("r1", "c1").is_none());
    assert_eq!(w.memory.visibility("big"), Visibility::Private);
}

#[test]
fn authority_is_checked_again_before_a_stored_receipt_is_returned() {
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.decide(&nonce, OWNER, true).unwrap();
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    // 이후 판본에서 스코프가 회수되면 영수증도 돌려주지 않는다
    w.publish(base("r2").build());
    assert!(matches!(share(&w, "c1", "m1"), Err(Error::Denied)));
    // 권한이 돌아오면 같은 영수증을 본다
    w.publish(share_snapshot("r1").build());
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    assert_eq!(w.memory.shared().len(), 1);
}

#[test]
fn an_old_button_is_stale_when_the_scope_or_the_policy_changed_after_the_card() {
    // 스코프가 회수됐다
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.publish(base("r2").build());
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    // 정책 판본이 바뀌었다(스코프는 그대로)
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.publish(share_snapshot("r2").build());
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert!(w.memory.shared().is_empty());
    // 승인 대기 중이던 호출은 같은 호출 ID로 다시 들어오면 새 조건의 새 카드를 연다
    let fresh = awaiting(share(&w, "c1", "m1"));
    assert_ne!(fresh, nonce);
}

#[test]
fn the_generic_start_never_runs_memory_share() {
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.decide(&nonce, OWNER, true).unwrap();
    let stored = w.ledger.stored_approval(&nonce);
    let start = StartCall {
        checked_revision: Revision("r1".into()),
        risk: Risk::Irreversible,
        tool_revision: Revision("builtin-1".into()),
        attempt: stored.approval.attempt.clone(),
        call: stored.approval.call.clone(),
        approval: Some((stored.approval, stored.reasons)),
        private_recall: true,
        owner: "x".into(),
    };
    assert!(block_on(w.ledger.start(start)).is_err());
    assert_eq!(w.memory.visibility("m1"), Visibility::Private);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
}

/// 승인까지 받아 공유를 한 번 끝낸 세계(`c1` 완료).
fn shared_once() -> World {
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.decide(&nonce, OWNER, true).unwrap();
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    w
}

#[test]
fn a_completed_call_replays_its_receipt_without_reading_the_text_even_after_it_changed() {
    let w = shared_once();
    let reads = w.memory.previews().len();
    // 본문이 고쳐지거나 지워져도 같은 호출 ID는 같은 영수증이고, 기억을 읽지도 다시 공유하지도 않는다
    w.memory.edit("m1", "Edited after it was shared.");
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    w.memory.remove("m1");
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    assert_eq!(
        w.memory.previews().len(),
        reads,
        "재전달은 본문을 읽지 않는다"
    );
    assert_eq!(w.memory.shared().len(), 1);
}

#[test]
fn a_call_id_that_finished_for_another_memory_or_tool_conflicts() {
    let w = shared_once();
    w.memory
        .add("m2", OWNER, Visibility::Private, "another note");
    assert!(matches!(share(&w, "c1", "m2"), Err(Error::Conflict)));
    // 같은 호출 ID를 다른 도구로 쓰려는 것도 같다
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(w.memory.shared().len(), 1);
}

#[test]
fn the_receipt_is_hidden_when_the_claim_or_the_revision_is_no_longer_current() {
    let w = shared_once();
    // 다른 프로세스가 새 판본을 게시했다(이 프로세스의 카탈로그는 그대로): 영수증을 내보내지 않는다
    w.ledger.set_active_revision("personal", "r2");
    assert!(matches!(share(&w, "c1", "m1"), Err(Error::Conflict)));
    w.ledger.set_active_revision("personal", "r1");
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    // claim이 넘어간 옛 시도에는 돌려주지 않는다(새 claim은 같은 영수증을 받는다)
    let old = w.ledger.claim_epoch("r1");
    w.ledger.bump_claim("r1");
    let stale = block_on(w.broker.call(ToolCall {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            generation: Generation(1),
            claim: Some(old),
        },
        call: CallId("c1".into()),
        tool: ToolId("memory.share".into()),
        args: JsonPayload(r#"{"id":"m1"}"#.into()),
    }));
    assert!(matches!(stale, Err(Error::Stale)), "{stale:?}");
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
}

#[test]
fn a_completion_that_commits_during_the_read_is_found_again_with_the_same_strict_rule() {
    // 미리보기가 실패하는 경쟁: 다른 재전달이 먼저 공유를 끝내고 기억이 지워졌다
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.decide(&nonce, OWNER, true).unwrap();
    let (ledger, memory) = (w.ledger.clone(), w.memory.clone());
    w.memory.before_next_preview(move || {
        ledger.force_done("r1", "c1", r#"{"id":"m1"}"#);
        memory.remove("m1");
    });
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    assert!(w.memory.shared().is_empty(), "이쪽에서는 공유하지 않았다");
    // begin이 해시 불일치로 실패하는 경쟁: 먼저 끝난 호출은 본문이 다른 채 읽혔다
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.decide(&nonce, OWNER, true).unwrap();
    let (ledger, memory) = (w.ledger.clone(), w.memory.clone());
    w.memory.before_next_preview(move || {
        ledger.force_done("r1", "c1", r#"{"id":"m1"}"#);
        memory.edit("m1", "Changed by someone else meanwhile.");
    });
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    // 끝난 호출이 다른 기억의 영수증이면 경쟁이어도 Conflict다
    let w = share_world();
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.decide(&nonce, OWNER, true).unwrap();
    let (ledger, memory) = (w.ledger.clone(), w.memory.clone());
    w.memory.before_next_preview(move || {
        ledger.force_done("r1", "c1", r#"{"id":"other"}"#);
        memory.remove("m1");
    });
    assert!(matches!(share(&w, "c1", "m1"), Err(Error::Conflict)));
}

#[test]
fn the_builtin_share_decision_needs_no_installed_tool_server() {
    let w = share_world();
    w.runner.set_image("demo-agenda", None); // 설치된 도구 서버가 하나도 없다
    let nonce = awaiting(share(&w, "c1", "m1"));
    w.decide(&nonce, OWNER, true).unwrap();
    assert_eq!(done(share(&w, "c1", "m1")), r#"{"id":"m1"}"#);
    assert_eq!(w.memory.visibility("m1"), Visibility::Profile);
}

#[test]
fn the_limit_in_the_share_flow_is_noted_with_that_very_call() {
    let w = World::with(cfg_with_rate(60, 1, 4));
    w.publish(share_snapshot("r1").build());
    w.ledger
        .add_run(run_context("personal", "r1", 1, OWNER, "main", &WITH_SHARE));
    w.memory.add("m1", OWNER, Visibility::Private, NOTE);
    // 첫 호출이 한도의 유일한 자리를 쓰고 승인 카드를 만든다
    let (first, limited) = w.call_noted("c1", "memory.share", r#"{"id":"m1"}"#);
    assert!(matches!(first, Ok(CallResult::Awaiting(_))) && !limited);
    // 두 번째 호출은 한도에 걸린다: 이 호출의 결과와 함께 한도 거절로 알린다
    let (refused, limited) = w.call_noted("c2", "memory.share", r#"{"id":"m1"}"#);
    assert!(matches!(refused, Err(Error::Unavailable)) && limited);
}
