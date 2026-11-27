//! Slack approval button tests: clicking user decides via `Broker::decide` and card refreshes from ledger state.
use crate::support::*;
use bot_core::{ledger::ApprovalState, port::Error};
use bot_gateway::testing::{
    ports::MemoryActive,
    slack::{Scripted, button_frame},
};
use serde_json::json;

fn card_json() -> String {
    json!({"nonce": "n1", "title": "Delete event", "lines": ["Target: Walk"]}).to_string()
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
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(
        rig.broker.decisions(),
        [("n1".to_string(), "slack:home/U1".to_string(), true)]
    );
    let update = &rig.slack.calls("chat.update")[0];
    assert_eq!(update.token, "test-bot-1");
    assert_eq!(update.body["channel"], "C1");
    assert_eq!(update.body["ts"], "5.5");
    assert_eq!(update.body["text"], "✅ Approved");
    let blocks = update.body["blocks"].as_array().unwrap();
    assert!(
        blocks.iter().all(|b| b["type"] != "actions"),
        "action buttons removed"
    );
    assert_eq!(blocks.last().unwrap()["elements"][0]["text"], "✅ Approved");
    assert!(ephemerals(&rig).is_empty());

    rig.ledger.set_state("n1", ApprovalState::Denied);
    rig.slack
        .push(button_frame(
            "B2", "T1", "C1", "U1", "5.5", "bot_deny", "n1",
        ))
        .await;
    assert!(rig.slack.until(|s| s.calls("chat.update").len() == 2).await);
    assert!(!rig.broker.decisions()[1].2);
    assert_eq!(rig.slack.calls("chat.update")[1].body["text"], "⛔ Denied");
}

#[tokio::test]
async fn clicks_that_cannot_decide_never_reach_the_broker_and_say_why_privately() {
    let rig = approval_rig(ApprovalState::Pending).await;
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
    // Unknown actions and clicks from other workspaces are ignored.
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
    // Each frame is handled independently; verify channel/user pairs and distinct notice types.
    let calls = rig.slack.calls("chat.postEphemeral");
    let mut notices: Vec<_> = calls
        .iter()
        .map(|call| {
            (
                call.body["channel"].as_str().expect("channel"),
                call.body["user"].as_str().expect("recipient"),
                call.body["text"].as_str().expect("notice text"),
            )
        })
        .collect();
    notices.sort_unstable();
    assert_eq!(notices.len(), 3);
    assert_eq!((notices[0].0, notices[0].1), ("C1", "U1"));
    assert_eq!((notices[1].0, notices[1].1), ("C1", "U9"));
    assert_eq!((notices[2].0, notices[2].1), ("D1", "U1"));
    assert_eq!(notices[0].2, notices[2].2);
    assert!(
        notices[0].2.to_ascii_lowercase().contains("unknown"),
        "{}",
        notices[0].2
    );
    assert!(
        notices[1].2.to_ascii_lowercase().contains("registered"),
        "{}",
        notices[1].2
    );
}

#[tokio::test]
async fn broker_refusals_are_explained_and_a_failed_card_update_never_reruns_the_decision() {
    let rig = approval_rig(ApprovalState::Pending).await;
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
    assert!(
        ephemerals(&rig)[0]
            .to_ascii_lowercase()
            .contains("approver"),
        "{}",
        ephemerals(&rig)[0]
    );
    assert!(rig.slack.calls("chat.update").is_empty());

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
    assert!(
        ephemerals(&rig)[1]
            .to_ascii_lowercase()
            .contains("no longer"),
        "{}",
        ephemerals(&rig)[1]
    );
    assert!(
        rig.slack.calls("chat.update")[0].body["text"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("expired")
    );

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
    assert!(
        ephemerals(&rig)[2].to_ascii_lowercase().contains("again"),
        "{}",
        ephemerals(&rig)[2]
    );

    // Failed card update must not re-run a committed decision.
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
        "must not retry card update"
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
    // Spans may be emitted after sending the ephemeral notice; wait for both to land.
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
    assert!(
        rig.slack.acks().is_empty(),
        "transient failure must not be acked"
    );
    assert!(rig.broker.decisions().is_empty());
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

    // Ledger lookup failure also avoids acking and never invokes decide.
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
    // Permanent rejections (denied or missing card) are acked.
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
        "must not retry card update"
    );
}

#[tokio::test]
async fn a_click_that_conflicts_with_a_final_decision_refreshes_the_card_and_is_acked() {
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
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(rig.slack.calls("chat.update")[0].body["text"], "⛔ Denied");
    let notices = ephemerals(&rig);
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].to_ascii_lowercase().contains("already"),
        "{}",
        notices[0]
    );
    assert!(
        rig.broker.decisions().is_empty(),
        "must not record a new decision"
    );
    assert_eq!(rig.broker.attempts(), 1);

    // If the ledger still shows Pending on Conflict, leave unacked for redelivery.
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
    assert!(rig.slack.acks().is_empty(), "stale process must not ack");
    assert_eq!(rig.broker.attempts(), 0, "stale process must not decide");
    assert!(
        rig.slack.calls("chat.update").is_empty(),
        "card must remain untouched"
    );
    active.set(Some("set-1"));
    rig.ledger.set_state("n1", ApprovalState::Approved);
    rig.slack.push(click()).await;
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(rig.broker.decisions().len(), 1);
}
