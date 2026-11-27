//! Verifies `posted_card` only selects DM approval cards confirmed as posted by FakeSlack (matching production
//! `Call::posted` rules), ignoring rejected, mismatched-channel, missing-ts, truncated, HTTP-error, or non-DM posts.
use crate::claude_support::*;
use bot_gateway::{
    slack::api::{Body, Raw, SlackApi},
    testing::slack::{FakeSlack, Scripted},
};
use serde_json::json;
use std::time::Duration;

fn card_message(channel: &str, nonce: &str) -> serde_json::Value {
    json!({"channel": channel, "text": "approval", "blocks": [{"type": "actions", "elements": [
        {"type": "button", "action_id": "bot_approve", "value": nonce},
        {"type": "button", "action_id": "bot_deny", "value": nonce}]}]})
}

#[tokio::test]
async fn only_a_successfully_posted_dm_card_is_pressed() {
    let slack = FakeSlack::start().await;
    let api = SlackApi::new(Some(&slack.base));
    slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_blocks"})),
    );
    let refused = card_message("D1", "n1");
    assert!(matches!(
        api.call("chat.postMessage", "token", Body::Json(&refused))
            .await,
        Raw::Error(_)
    ));
    let rejected = slack.calls("chat.postMessage");
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].status, 200);
    assert!(!rejected[0].succeeded(), "{:?}", rejected[0].reply);
    let waited = tokio::time::timeout(Duration::from_millis(700), posted_card(&slack, "n1")).await;
    assert!(waited.is_err(), "must not select a rejected card");

    // Ignore unconfirmed replies: wrong channel, missing ts, truncation warning, or 502 with ok:true.
    for scripted in [
        Scripted::Json(json!({"ok": true, "channel": "D2", "ts": "1700000000.000009"})),
        Scripted::Json(json!({"ok": true, "channel": "D1"})),
        Scripted::Json(
            json!({"ok": true, "channel": "D1", "ts": "1700000000.000009",
            "response_metadata": {"warnings": ["message_truncated"]}}),
        ),
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: r#"{"ok":true,"channel":"D1","ts":"1700000000.000009"}"#.into(),
        },
    ] {
        slack.script("chat.postMessage", scripted);
        let unconfirmed = card_message("D1", "n1");
        let _ = api
            .call("chat.postMessage", "token", Body::Json(&unconfirmed))
            .await;
    }
    assert_eq!(slack.calls("chat.postMessage").len(), 5);
    let waited = tokio::time::timeout(Duration::from_millis(700), posted_card(&slack, "n1")).await;
    assert!(waited.is_err(), "must not select an unconfirmed card");

    let team = card_message("C1", "n1");
    assert!(matches!(
        api.call("chat.postMessage", "token", Body::Json(&team))
            .await,
        Raw::Ok(_)
    ));
    let waited = tokio::time::timeout(Duration::from_millis(700), posted_card(&slack, "n1")).await;
    assert!(waited.is_err(), "must not select a non-DM channel card");

    let ok = card_message("D1", "n1");
    assert!(matches!(
        api.call("chat.postMessage", "token", Body::Json(&ok)).await,
        Raw::Ok(_)
    ));
    let posted = posted_card(&slack, "n1").await;
    let answered = slack
        .calls("chat.postMessage")
        .into_iter()
        .find(|call| call.posted().is_some_and(|(channel, _)| channel == "D1"))
        .unwrap();
    assert_eq!(posted.channel, "D1");
    assert_eq!(posted.ts, answered.reply["ts"].as_str().unwrap());
    assert_eq!(
        answered.posted(),
        Some(("D1".to_owned(), posted.ts.clone()))
    );
    assert_eq!(posted.value, "n1");
    let waited = tokio::time::timeout(Duration::from_millis(700), posted_card(&slack, "n2")).await;
    assert!(waited.is_err());
}
