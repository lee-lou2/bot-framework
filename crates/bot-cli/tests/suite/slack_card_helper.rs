//! 종단 시험이 승인 버튼을 찾는 도우미(`posted_card`)는 Slack에 **성공으로 게시된** 카드의 채널·`ts`·버튼만 쓴다. 가짜 Slack은 응답을 내보내기 전에 요청을 기록하므로
//! (거절된 요청도 기록에 남는다) 기록만 보면 게시되지 않은 카드의 버튼을 누르게 된다 — 그 길을 막는 시험이다. DB·claude는 필요 없다.

use crate::claude_support::*;
use bot_gateway::{
    slack::api::{Body, Raw, SlackApi},
    testing::slack::{FakeSlack, Scripted},
};
use serde_json::json;
use std::time::Duration;

fn card_message(channel: &str, nonce: &str) -> serde_json::Value {
    json!({"channel": channel, "text": "승인", "blocks": [{"type": "actions", "elements": [
        {"type": "button", "action_id": "bot_approve", "value": nonce},
        {"type": "button", "action_id": "bot_deny", "value": nonce}]}]})
}

#[tokio::test]
async fn only_a_successfully_posted_dm_card_is_pressed() {
    let slack = FakeSlack::start().await;
    let api = SlackApi::new(Some(&slack.base));
    // 거절된 요청: 가짜 서버는 요청을 기록하지만 카드는 게시되지 않았다
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
    assert!(waited.is_err(), "거절된 카드의 버튼을 찾았어요");

    // 운영 발송기가 «올라갔다»로 확정하지 않을 응답의 카드도 고르지 않는다: 요청과 다른 DM을 말하는 응답·`ts` 누락·잘림 경고·HTTP 502에 ok:true
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
    assert!(waited.is_err(), "확정되지 않은 응답의 카드를 찾았어요");

    // 다른 채널(팀 채널)로 성공한 카드도 소유자 DM 카드가 아니다
    let team = card_message("C1", "n1");
    assert!(matches!(
        api.call("chat.postMessage", "token", Body::Json(&team))
            .await,
        Raw::Ok(_)
    ));
    let waited = tokio::time::timeout(Duration::from_millis(700), posted_card(&slack, "n1")).await;
    assert!(waited.is_err(), "DM이 아닌 채널의 카드를 찾았어요");

    // 성공으로 게시된 DM 카드: 가짜 서버가 응답한 채널·ts·버튼 값을 읽는다
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
    // 다른 승인의 카드는 아니다
    let waited = tokio::time::timeout(Duration::from_millis(700), posted_card(&slack, "n2")).await;
    assert!(waited.is_err());
}
