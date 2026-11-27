//! Slack 승인 버튼 수락 시험: 클릭한 Slack 사용자가 주체로 `Broker::decide`에 가고 원장 상태로 카드를 갱신한다.
use crate::support::*;
use bot_core::{ledger::ApprovalState, port::Error};
use bot_gateway::testing::{
    ports::MemoryActive,
    slack::{Scripted, button_frame},
};
use serde_json::json;
fn card_json() -> String {
    json!({"nonce": "n1", "title": "일정 삭제", "lines": ["대상: 산책"]}).to_string()
}

async fn approval_rig(state: ApprovalState) -> Rig {
    let rig = connected(vec![personal()]).await;
    rig.ledger.put("n1", "personal", state, &card_json());
    rig
}

fn ephemerals(rig: &Rig) -> Vec<String> {
    rig.slack
        .calls("chat.postEphemeral")
        .iter()
        .map(|c| c.body["text"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn an_approve_click_decides_as_the_clicking_user_and_refreshes_the_card() {
    let rig = approval_rig(ApprovalState::Approved).await;
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| !s.calls("chat.update").is_empty())
            .await
    );
    // 판정이 끝나면 ack, 클릭한 Slack 사용자가 주체
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(
        rig.broker.decisions(),
        [("n1".to_string(), "slack:home/U1".to_string(), true)]
    );
    let update = &rig.slack.calls("chat.update")[0];
    assert_eq!(update.token, "test-bot-1");
    assert_eq!(update.body["channel"], "C1");
    assert_eq!(update.body["ts"], "5.5");
    assert_eq!(update.body["text"], "✅ 승인됨");
    let blocks = update.body["blocks"].as_array().unwrap();
    assert!(
        blocks.iter().all(|b| b["type"] != "actions"),
        "버튼은 떼어진다"
    );
    assert_eq!(blocks.last().unwrap()["elements"][0]["text"], "✅ 승인됨");
    assert!(ephemerals(&rig).is_empty());

    // 거절 클릭은 allow=false
    rig.ledger.set_state("n1", ApprovalState::Denied);
    rig.slack
        .push(button_frame(
            "B2", "T1", "C1", "U1", "5.5", "bot_deny", "n1",
        ))
        .await;
    assert!(rig.slack.until(|s| s.calls("chat.update").len() == 2).await);
    assert!(!rig.broker.decisions()[1].2);
    assert_eq!(rig.slack.calls("chat.update")[1].body["text"], "⛔ 거절됨");
}

#[tokio::test]
async fn clicks_that_cannot_decide_never_reach_the_broker_and_say_why_privately() {
    let rig = approval_rig(ApprovalState::Pending).await;
    // 등록되지 않은 사람
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "C1",
            "U9",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    // 다른 프로필의 카드·없는 카드
    rig.ledger
        .put("n2", "work", ApprovalState::Pending, &card_json());
    rig.slack
        .push(button_frame(
            "B2",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n2",
        ))
        .await;
    rig.slack
        .push(button_frame(
            "B3",
            "T1",
            "D1",
            "U1",
            "5.5",
            "bot_approve",
            "nope",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 3)
            .await
    );
    assert!(rig.broker.decisions().is_empty());
    // 모르는 버튼·다른 워크스페이스의 클릭은 아무 일도 하지 않는다
    rig.slack
        .push(button_frame(
            "B4",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_other",
            "n1",
        ))
        .await;
    rig.slack
        .push(button_frame(
            "B5",
            "T9",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 5).await);
    settle().await;
    assert!(rig.broker.decisions().is_empty());
    // 각 프레임은 독립적으로 처리한다. 같은 문구를 받는 두 클릭도 채널로 구별해 각각 한 번만 전달됐는지 확인한다.
    let calls = rig.slack.calls("chat.postEphemeral");
    let mut notices: Vec<_> = calls
        .iter()
        .map(|call| {
            (
                call.body["channel"].as_str().expect("채널"),
                call.body["user"].as_str().expect("수신자"),
                call.body["text"].as_str().expect("안내 문구"),
            )
        })
        .collect();
    notices.sort_unstable();
    assert_eq!(
        notices,
        [
            ("C1", "U1", "알 수 없는 승인 카드예요"),
            ("C1", "U9", "이 승인은 등록된 사용자만 누를 수 있어요"),
            ("D1", "U1", "알 수 없는 승인 카드예요"),
        ]
    );
}

#[tokio::test]
async fn broker_refusals_are_explained_and_a_failed_card_update_never_reruns_the_decision() {
    let rig = approval_rig(ApprovalState::Pending).await;
    // 지정된 승인자가 아니면 거절(카드는 그대로)
    rig.broker.fail_next(Error::Denied);
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 1)
            .await
    );
    assert!(ephemerals(&rig)[0].contains("승인자"));
    assert!(rig.slack.calls("chat.update").is_empty());
    // 만료된 카드(Stale): 안내하고 원장 상태로 카드를 닫는다
    rig.broker.fail_next(Error::Stale);
    rig.ledger.set_state("n1", ApprovalState::Expired);
    rig.slack
        .push(button_frame(
            "B2",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| !s.calls("chat.update").is_empty())
            .await
    );
    assert!(ephemerals(&rig)[1].contains("유효하지 않아요"));
    assert!(
        rig.slack.calls("chat.update")[0].body["text"]
            .as_str()
            .unwrap()
            .contains("만료")
    );
    // 일시 장애는 다시 누르게 안내한다
    rig.broker.fail_next(Error::Unavailable);
    rig.slack
        .push(button_frame(
            "B3",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 3)
            .await
    );
    assert!(ephemerals(&rig)[2].contains("다시 눌러"));

    // 카드 갱신이 실패해도 결정은 한 번만 기록된다
    rig.ledger.set_state("n1", ApprovalState::Approved);
    rig.slack.script(
        "chat.update",
        Scripted::Raw {
            status: 500,
            retry_after: None,
            body: "boom".into(),
        },
    );
    let before = rig.broker.decisions().len();
    rig.slack
        .push(button_frame(
            "B4",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(rig.slack.until(|s| s.calls("chat.update").len() == 2).await);
    settle().await;
    assert_eq!(rig.broker.decisions().len(), before + 1);
    assert_eq!(
        rig.slack.calls("chat.update").len(),
        2,
        "갱신을 다시 시도하지 않는다"
    );
}

#[tokio::test]
async fn approval_clicks_are_traced_without_the_nonce() {
    use bot_core::trace::{Val, key};
    let rig = approval_rig(ApprovalState::Approved).await;
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| !s.calls("chat.update").is_empty())
            .await
    );
    rig.broker.fail_next(Error::Denied);
    rig.slack
        .push(button_frame(
            "B2", "T1", "C1", "U1", "5.5", "bot_deny", "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 1)
            .await
    );
    // 스팬은 안내를 보낸 뒤에 기록될 수 있다: 기록될 때까지(최대 5초) 기다린 다음 센다
    assert!(rig.tracer.until_named("approval decide", 2).await);
    let spans = rig.tracer.named("approval decide");
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].str(key::BOT_APPROVAL_OUTCOME), Some("approved"));
    assert_eq!(spans[0].str(key::BOT_PRINCIPAL), Some("slack:home/U1"));
    assert_eq!(spans[1].str(key::BOT_APPROVAL_OUTCOME), Some("forbidden"));
    for span in &spans {
        assert!(span.attrs.iter().all(|(k, v)| key::ALLOWED.contains(k)
            && !matches!(v, Val::Str(text) if text.contains("n1"))));
    }
}

#[tokio::test]
async fn a_transient_failure_is_not_acked_and_the_redelivered_click_decides_once() {
    let rig = approval_rig(ApprovalState::Approved).await;
    // 브로커 장애: ack하지 않고 다시 누르라고 안내한다
    rig.broker.fail_next(Error::Unavailable);
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 1)
            .await
    );
    settle().await;
    assert!(rig.slack.acks().is_empty(), "일시 실패는 ack하지 않는다");
    assert!(rig.broker.decisions().is_empty());
    // 같은 버튼이 다시 오면 결정이 한 번 기록되고 그제야 ack한다
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"B1".to_string()))
            .await
    );
    assert_eq!(rig.broker.decisions().len(), 1);
    assert_eq!(rig.broker.attempts(), 2);

    // 원장 조회 장애도 ack하지 않고 결정을 시도하지도 않는다
    let rig = approval_rig(ApprovalState::Pending).await;
    rig.ledger.fail_next(Error::Unavailable);
    rig.slack
        .push(button_frame(
            "B2",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 1)
            .await
    );
    settle().await;
    assert!(rig.slack.acks().is_empty());
    assert_eq!(rig.broker.attempts(), 0);
    // 영구적으로 처리할 수 없는 클릭(승인자가 아님·없는 카드)은 ack한다
    rig.broker.fail_next(Error::Denied);
    rig.slack
        .push(button_frame(
            "B3",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    rig.slack
        .push(button_frame(
            "B4",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "missing",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 2).await);
}

#[tokio::test]
async fn a_failed_card_update_after_a_committed_decision_is_still_acked() {
    let rig = approval_rig(ApprovalState::Approved).await;
    rig.slack.script(
        "chat.update",
        Scripted::Raw {
            status: 500,
            retry_after: None,
            body: "boom".into(),
        },
    );
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"B1".to_string()))
            .await
    );
    assert_eq!(rig.broker.decisions().len(), 1);
    assert_eq!(
        rig.slack.calls("chat.update").len(),
        1,
        "갱신을 다시 시도하지 않는다"
    );
}

#[tokio::test]
async fn a_click_that_conflicts_with_a_final_decision_refreshes_the_card_and_is_acked() {
    // 이미 거절이 확정된 카드에 승인을 누른다: 브로커는 Conflict를 돌려준다
    let rig = approval_rig(ApprovalState::Denied).await;
    rig.broker.fail_next(Error::Conflict);
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    // 재전달을 기다리지 않는다: 원장의 최종 상태로 카드를 갱신하고 ack하며, 누른 사람에게만 알린다
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(rig.slack.calls("chat.update")[0].body["text"], "⛔ 거절됨");
    assert_eq!(ephemerals(&rig), ["이미 다른 결정이 확정된 카드예요"]);
    assert!(rig.broker.decisions().is_empty(), "새 결정은 만들지 않는다");
    assert_eq!(rig.broker.attempts(), 1);

    // 원장이 아직 대기라고 하면(확정을 확인하지 못함) 재전달에 맡긴다
    let rig = approval_rig(ApprovalState::Pending).await;
    rig.broker.fail_next(Error::Conflict);
    rig.slack
        .push(button_frame(
            "B2",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 1)
            .await
    );
    settle().await;
    assert!(rig.slack.acks().is_empty());
    assert!(rig.slack.calls("chat.update").is_empty());
}

#[tokio::test]
async fn a_stale_process_leaves_the_card_untouched_and_does_not_ack() {
    // 저장소의 활성 집합(set-2)이 이 프로세스가 시작한 집합(set-1)과 다르다
    let active = MemoryActive::new(Some("set-2"));
    let rig = rig_checked(
        vec![personal()],
        default_secrets(),
        host(""),
        Some(active.check("set-1")),
        |_| {},
    )
    .await;
    assert!(rig.slack.connected().await);
    rig.ledger
        .put("n1", "personal", ApprovalState::Pending, &card_json());
    let click = || button_frame("B1", "T1", "C1", "U1", "5.5", "bot_approve", "n1");
    rig.slack.push(click()).await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 1)
            .await
    );
    settle().await;
    assert!(rig.slack.acks().is_empty(), "구판은 ack하지 않는다");
    assert_eq!(rig.broker.attempts(), 0, "구판은 결정을 내리지 않는다");
    assert!(
        rig.slack.calls("chat.update").is_empty(),
        "카드 상태를 그대로 둔다"
    );
    // 새 집합과 같아지면 같은 클릭이 한 번 결정되고 ack된다
    active.set(Some("set-1"));
    rig.ledger.set_state("n1", ApprovalState::Approved);
    rig.slack.push(click()).await;
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(rig.broker.decisions().len(), 1);
}
