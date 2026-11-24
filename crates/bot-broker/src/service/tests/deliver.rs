//! 예약 회신 전송(`schedule.deliver`): 호스트 전용 입구 `ScheduleDelivery::release`.
//! 승인자는 예약을 만든 본인이고, 카드에는 수신 경로와 모든 답이 한 글자도 줄이지 않고 담기며, 저장소가 승인 소비·outbox·완료를 한 커밋으로 쓴다.

use super::{
    schedule::{OWNER, WITH_SCHEDULE, at_args, schedule_world, snapshot_with},
    *,
};
use crate::canon::sha256;
use bot_core::{
    command::JsonPayload,
    ledger::{ApprovalState, CallLedger, CallPhase, StartCall},
    schedule::{
        DELIVER_CALL, DELIVER_REVISION, DELIVER_TOOL, DELIVERY_CARD_LABEL, DeliveryDraft,
        DeliveryTarget, ScheduleDelivery,
    },
    state::{Conversation, ConversationKind, Risk},
    trace::key,
};

pub(super) const RUN: &str = "r1";
pub(super) const TEXT: &str = "월간 보고서를 보냅니다";

fn draft_of(answers: &[&str]) -> DeliveryDraft {
    DeliveryDraft {
        activation: Revision("set-a".into()),
        target: DeliveryTarget {
            binding: BindingId("team".into()),
            conversation: Conversation {
                kind: ConversationKind::Channel,
                id: "channel-a".into(),
                thread: None,
            },
        },
        answers: answers.iter().map(|a| JsonPayload((*a).into())).collect(),
    }
}

fn text_answer(text: &str) -> String {
    serde_json::json!({ "text": text }).to_string()
}

pub(super) fn draft(text: &str) -> DeliveryDraft {
    draft_of(&[&text_answer(text)])
}

pub(super) fn deliver_world() -> World {
    let w = schedule_world();
    w.replies.set_draft(RUN, draft(TEXT));
    w
}

pub(super) fn attempt_of(w: &World, run: &str) -> Attempt {
    Attempt {
        profile: ProfileId("personal".into()),
        run: RunId(run.into()),
        claim: Some(w.ledger.claim_epoch(run)),
        generation: Generation(1),
    }
}

pub(super) fn release_on(
    w: &World,
    broker: &BrokerService,
    run: &str,
) -> Result<CallResult, Error> {
    block_on(broker.release(attempt_of(w, run)))
}

pub(super) fn release(w: &World) -> Result<CallResult, Error> {
    release_on(w, &w.broker, RUN)
}

fn refused(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("schedule.refuse".into(), word.into()))
}

/// 카드의 `전송 초안` 행을 이어 붙인 값(이어지는 줄은 공백 두 칸으로 시작한다).
fn card_document(w: &World, nonce: &ApprovalId) -> String {
    let card = w.ledger.stored_approval(nonce).card.0;
    let value: serde_json::Value = serde_json::from_str(&card).expect("카드 JSON");
    let lines: Vec<&str> = value["approval"]["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| line.as_str().expect("줄"))
        .collect();
    let label = format!("{DELIVERY_CARD_LABEL}: ");
    let first = lines
        .iter()
        .position(|line| line.starts_with(&label))
        .expect("전송 초안 행");
    let mut out = lines[first][label.len()..].to_owned();
    for line in &lines[first + 1..] {
        match line.strip_prefix("  ") {
            Some(rest) => out.push_str(rest),
            None => break,
        }
    }
    out
}

#[test]
fn the_owner_sees_the_whole_draft_and_it_is_sent_once_after_approval() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    // 승인 전에는 아무것도 보내지 않는다
    assert!(w.replies.outbox().is_empty());
    assert!(matches!(
        w.ledger.phase(RUN, DELIVER_CALL),
        CallPhase::Open { approval: Some(_) }
    ));
    // 승인 결합값: 예약을 만든 본인이 요청자이자 승인자, 되돌릴 수 없는 호출, 승인 문서의 해시
    let stored = w.ledger.stored_approval(&nonce);
    let expected = draft(TEXT);
    assert_eq!(stored.approval.tool.0, DELIVER_TOOL);
    assert_eq!(stored.approval.tool_revision.0, DELIVER_REVISION);
    assert_eq!(stored.approval.call.0, DELIVER_CALL);
    assert_eq!(stored.approval.risk, Risk::Irreversible);
    assert_eq!(
        (
            stored.approval.requester.0.as_str(),
            stored.approval.approver.0.as_str()
        ),
        (OWNER, OWNER)
    );
    assert_eq!(stored.approval.args_sha256, expected.sha256().unwrap());
    assert_eq!(stored.reasons, vec!["irreversible".to_string()]);
    // 카드: 도구·승인자·수신 경로와 모든 답이 든 문서 전체(해시가 이 문서에서 나왔다)
    let document = card_document(&w, &nonce);
    assert_eq!(document, expected.approval_json().unwrap());
    assert_eq!(sha256(&document), stored.approval.args_sha256);
    assert!(document.contains(TEXT) && document.contains("channel-a") && document.contains("team"));
    assert!(
        stored.card.0.contains("도구: schedule.deliver"),
        "{}",
        stored.card.0
    );
    // 승인하면 한 번만 보낸다
    w.decide(&nonce, OWNER, true).unwrap();
    let receipt = done(release(&w));
    // 영수증은 저장소가 정한 `{"queued":N}`(N은 outbox에 등록한 답 개수)이고 브로커는 그 바이트를 그대로 돌려준다
    assert_eq!(receipt, r#"{"queued":1}"#);
    assert_eq!(receipt, expected.queued_receipt().0);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    let sent = w.replies.outbox();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].binding.as_str(), sent[0].conversation.as_str()),
        ("team", "channel-a")
    );
    assert_eq!(sent[0].answers, vec![text_answer(TEXT)]);
    // 재시도는 같은 영수증이고 다시 보내지 않는다(초안을 다시 읽지도 않는다)
    assert_eq!(done(release(&w)), receipt);
    w.replies.set_draft(RUN, draft("나중에 달라진 내용"));
    assert_eq!(done(release(&w)), receipt);
    assert_eq!(w.replies.outbox().len(), 1);
    // 스팬: 호스트 전용 호출도 일반 호출과 같은 모양으로 남고 내용은 싣지 않는다
    let spans = w.tracer.spans();
    assert!(
        spans
            .iter()
            .all(|s| s.str(key::GEN_AI_TOOL_NAME) == Some(DELIVER_TOOL))
    );
    assert_eq!(spans[0].str(key::BOT_CALL_STATE), Some("awaiting"));
    assert_eq!(spans[0].str(key::BOT_TOOL_RISK), Some("irreversible"));
    assert!(format!("{spans:?}").find(TEXT).is_none());
}

#[test]
fn a_pending_card_is_the_same_card_until_it_is_decided() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    assert_eq!(awaiting(release(&w)), nonce, "새 카드를 또 열지 않는다");
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn every_answer_and_attachment_is_on_the_card_in_order_or_nothing_is_sent() {
    let w = schedule_world();
    let first = text_answer("첫 회신");
    let second = serde_json::json!({
        "text": "다음",
        "attachments": [{"name": "note.txt", "type": "text/plain", "data": "b25lCnR3bw=="}]
    })
    .to_string();
    w.replies.set_draft(RUN, draft_of(&[&first, &second]));
    let nonce = awaiting(release(&w));
    let document = card_document(&w, &nonce);
    let (a, b) = (
        document.find("첫 회신").unwrap(),
        document.find("다음").unwrap(),
    );
    assert!(a < b, "답은 보낼 순서대로 보인다: {document}");
    assert!(
        document.contains(r#""text":"one\ntwo""#),
        "첨부 원문이 보인다: {document}"
    );
    w.decide(&nonce, OWNER, true).unwrap();
    // 영수증의 N은 등록한 답의 개수다(첨부는 그 답에 포함된다)
    assert_eq!(done(release(&w)), r#"{"queued":2}"#);
    assert_eq!(w.replies.outbox()[0].answers, vec![first, second]);
}

#[test]
fn a_draft_the_card_cannot_show_in_full_is_refused_without_any_record() {
    let image = serde_json::json!({
        "text": "그림",
        "attachments": [{"name": "a.png", "type": "image/png", "data": "aGk="}]
    })
    .to_string();
    for answers in [
        vec![image],
        vec![text_answer("정상"), r#"{"text":"x","extra":1}"#.to_owned()],
        vec![r#"{"attachments":[]}"#.to_owned()],
        vec![],
    ] {
        let w = schedule_world();
        let refs: Vec<&str> = answers.iter().map(String::as_str).collect();
        w.replies.set_draft(RUN, draft_of(&refs));
        assert!(matches!(release(&w), Err(Error::Denied)), "{answers:?}");
        assert!(refused(&w, "card_unsupported"));
        assert!(!w.ledger.has_call(RUN, DELIVER_CALL), "기록하지 않는다");
        assert!(w.replies.outbox().is_empty());
    }
}

#[test]
fn the_card_line_limit_is_checked_before_recording_and_the_edge_fits_exactly() {
    // 문서 자체는 core 한도 안인데 카드의 실제 줄 한도(머리줄 포함)를 넘는 크기: 일부만 보여 주지 않고 거절한다.
    let w = schedule_world();
    w.replies.set_draft(RUN, draft(&"가".repeat(2500)));
    assert!(draft(&"가".repeat(2500)).approval_json().is_ok());
    assert!(matches!(release(&w), Err(Error::Denied)));
    assert!(refused(&w, "card_unsupported"));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    // 경계: 카드에 다 담기는 가장 큰 크기까지는 전체를 보여 주고, 하나 더 크면 거절한다
    let fits = |n: usize| {
        let w = schedule_world();
        w.replies.set_draft(RUN, draft(&"x".repeat(n)));
        match release(&w) {
            Ok(CallResult::Awaiting(nonce)) => {
                assert_eq!(
                    card_document(&w, &nonce),
                    draft(&"x".repeat(n)).approval_json().unwrap(),
                    "{n}자는 전부 보여야 한다"
                );
                true
            }
            Err(Error::Denied) => false,
            other => panic!("{n}: {other:?}"),
        }
    };
    let (mut low, mut high) = (1usize, 9000usize);
    assert!(fits(low) && !fits(high));
    while high - low > 1 {
        let mid = (low + high) / 2;
        if fits(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    assert!(fits(low) && !fits(low + 1), "경계 {low}");
}

#[test]
fn without_a_frozen_draft_or_a_store_nothing_happens() {
    // 동결한 초안이 없다(일반 실행·원래 대화로 보내는 답)
    let w = schedule_world();
    assert!(matches!(release(&w), Err(Error::Missing)));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    // 초안 저장소를 붙이지 않았다
    let w = deliver_world();
    let bare = w.bare();
    assert!(matches!(
        release_on(&w, &bare, RUN),
        Err(Error::Unavailable)
    ));
    assert!(!w.ledger.has_call(RUN, DELIVER_CALL));
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn only_the_human_who_made_the_schedule_can_release_it() {
    let w = deliver_world();
    // 서비스 주체의 실행
    w.ledger.add_run(run_context(
        "personal",
        "r2",
        1,
        "service:morning",
        "main",
        &WITH_SCHEDULE,
    ));
    w.replies.set_draft("r2", draft(TEXT));
    assert!(matches!(
        release_on(&w, &w.broker, "r2"),
        Err(Error::Denied)
    ));
    assert!(refused(&w, "not_human"));
    // 승인자를 확인할 수 없는 실행(예약 출처가 없다)
    let mut ctx = run_context("personal", "r3", 1, OWNER, "main", &WITH_SCHEDULE);
    ctx.conversation.kind = ConversationKind::Scheduled;
    w.ledger.add_run(ctx);
    w.replies.set_draft("r3", draft(TEXT));
    assert!(matches!(
        release_on(&w, &w.broker, "r3"),
        Err(Error::Denied)
    ));
    assert!(refused(&w, "not_owner"));
    // 예약 출처의 승인자가 요청자와 다른 사람이면 요청자가 곧 승인자인 이 입구에서는 승인자가 없는 것이다
    w.publish(
        snapshot_with("r1b")
            .cron_trigger("main", "alice-job", "local:alice", Some(OWNER))
            .build(),
    );
    let mut ctx = run_context("personal", "r4", 1, "local:alice", "main", &WITH_SCHEDULE);
    ctx.conversation.kind = ConversationKind::Scheduled;
    ctx.trigger = Some(bot_core::state::TriggerRef {
        agent: AgentId("main".into()),
        id: "alice-job".into(),
    });
    w.ledger.add_run(ctx);
    w.replies.set_draft("r4", draft(TEXT));
    assert!(matches!(
        release_on(&w, &w.broker, "r4"),
        Err(Error::Denied)
    ));
    assert!(!w.ledger.has_call("r4", DELIVER_CALL));
    // 예약을 만들 때 필요했던 스코프가 지금 없다
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(release(&w), Err(Error::Denied)));
    assert!(refused(&w, "scope_missing"));
    for run in ["r1", "r2", "r3", "r4"] {
        assert!(
            !w.ledger.has_call(run, DELIVER_CALL),
            "{run}: 기록하지 않는다"
        );
    }
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn only_the_designated_human_decides_and_a_stale_button_changes_nothing() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    // 다른 사람은 승인할 수 없다
    assert!(matches!(
        w.decide(&nonce, "local:alice", true),
        Err(Error::Denied)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    // 카드를 띄운 뒤 판본이 바뀌었다: 옛 버튼은 거절된다
    w.publish(snapshot_with("r2").build());
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert!(w.replies.outbox().is_empty());
    // 스코프가 철회된 판본에서는 승인해도 닫힌다
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    assert!(w.replies.outbox().is_empty());
}

#[test]
fn an_owner_rejection_ends_the_occurrence_as_rejected_and_an_expired_approval_is_only_denied() {
    // 소유자가 거절했다: 이 발생을 «거절»로 끝낸다(영수증이 결과이고 보내지 않으며 안내도 없다)
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, false).unwrap();
    let receipt = done(release(&w));
    assert_eq!(
        receipt,
        bot_core::schedule::UnsentReason::Rejected.receipt().0
    );
    assert!(w.replies.outbox().is_empty());
    assert!(w.replies.notices().is_empty());
    assert!(matches!(
        w.ledger.phase(RUN, DELIVER_CALL),
        CallPhase::Done(_)
    ));
    // 재시도는 같은 영수증이다
    assert_eq!(done(release(&w)), receipt);
    // 승인했지만 만료됐다: 소유자가 거절한 것이 아니다(기존 거절)
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.clock.advance(Duration::from_secs(31 * 60));
    assert!(matches!(release(&w), Err(Error::Denied)));
    assert!(w.replies.outbox().is_empty());
    assert!(w.replies.notices().is_empty());
    assert_ne!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
}

#[test]
fn a_draft_or_recipient_that_changed_after_the_card_is_never_sent_and_keeps_the_approval() {
    // 카드를 본 뒤 초안이 달라졌다: 새 문서의 지문이 호출에 묶인 지문과 달라 이어가지 못한다
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.replies.set_draft(RUN, draft("전혀 다른 내용"));
    assert!(matches!(release(&w), Err(Error::Conflict)));
    assert!(w.replies.outbox().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    // 원래 초안이면 같은 승인으로 보낼 수 있다
    w.replies.set_draft(RUN, draft(TEXT));
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
    // 기록과 커밋 사이에 초안이 달라졌다(저장소가 지금 초안으로 지문을 다시 계산한다)
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    let replies = w.replies.clone();
    w.ledger
        .set_after_begin(move || replies.set_draft(RUN, draft("커밋 직전에 바뀐 내용")));
    assert!(matches!(release(&w), Err(Error::Conflict)));
    assert!(w.replies.outbox().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    // 수신 경로를 정하는 활성 집합이 달라졌다
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    w.replies.set_active_set("set-b");
    assert!(matches!(release(&w), Err(Error::Conflict)));
    assert!(w.replies.outbox().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
}

#[test]
fn the_general_path_can_never_preclaim_the_reserved_call_id() {
    let w = deliver_world();
    let create = at_args("x");
    for (tool, args) in [
        ("agenda.list", "{}"),
        ("schedule.deliver", "{}"),
        ("schedule.create", create.as_str()),
        ("schedule.list", "{}"),
        ("memory.share", r#"{"id":"m1"}"#),
        ("not.a.tool", "{}"),
    ] {
        assert!(
            matches!(w.call(DELIVER_CALL, tool, args), Err(Error::Denied)),
            "{tool}"
        );
    }
    assert!(
        !w.ledger.has_call(RUN, DELIVER_CALL),
        "어떤 도구로도 기록하지 않는다"
    );
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.refuse".into(), "reserved_call".into()))
    );
    // 모델이 호스트 전용 도구를 다른 호출 ID로 불러도 없는 것처럼 답한다
    assert!(matches!(
        w.call("c1", "schedule.deliver", "{}"),
        Err(Error::Missing)
    ));
    // 막힌 뒤에도 호스트 전용 입구는 그대로 열린다
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    done(release(&w));
    assert_eq!(w.replies.outbox().len(), 1);
}

#[test]
fn the_generic_start_path_cannot_run_a_delivery_call() {
    let w = deliver_world();
    let nonce = awaiting(release(&w));
    w.decide(&nonce, OWNER, true).unwrap();
    let start = StartCall {
        checked_revision: Revision("r1".into()),
        risk: Risk::Irreversible,
        tool_revision: Revision(DELIVER_REVISION.into()),
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId(RUN.into()),
            claim: Some(w.ledger.claim_epoch(RUN)),
            generation: Generation(1),
        },
        call: CallId(DELIVER_CALL.into()),
        approval: None,
        private_recall: false,
        owner: "x".into(),
    };
    assert!(matches!(
        block_on(w.ledger.start(start)),
        Err(Error::Denied)
    ));
    assert!(w.replies.outbox().is_empty());
}
