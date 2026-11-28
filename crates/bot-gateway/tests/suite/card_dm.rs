//! Owner-DM approval card posting behavior against fake Slack (`conversations.open` and `chat.postMessage`).
use crate::delivery_support::*;
use crate::support;
use bot_core::{
    cards::{CardSource, LABEL_RECIPIENT},
    id::{PrincipalId, ProfileId},
    intake::NotSentReason,
};
use bot_gateway::{
    reply::Card,
    slack::send::{CardPost, slack_user},
    testing::slack::Scripted,
};
use serde_json::{Value, json};
use std::time::Duration;

async fn fresh() -> Rig {
    rig().await
}

fn owner(text: &str) -> PrincipalId {
    PrincipalId(text.to_owned())
}

fn card() -> Card {
    Card {
        nonce: "n-1".into(),
        title: "Scheduled delivery approval".into(),
        lines: vec![
            format!("{LABEL_RECIPIENT}: Team channel"),
            "Instruction: Notify when data arrives".into(),
        ],
    }
}

fn personal() -> ProfileId {
    ProfileId("personal".into())
}

async fn post(rig: &Rig, who: &PrincipalId, card: &Card) -> CardPost {
    let sender = rig.sender();
    sender
        .post_owner_card(&personal(), "home", who, card, None)
        .await
}

fn open_calls(rig: &Rig) -> Vec<bot_gateway::testing::slack::Call> {
    rig.slack.calls("conversations.open")
}

fn card_posts(rig: &Rig) -> Vec<bot_gateway::testing::slack::Call> {
    rig.slack.calls("chat.postMessage")
}

#[tokio::test]
async fn the_whole_card_goes_to_the_owners_dm_once_and_the_receipt_is_what_slack_answered() {
    let rig = fresh().await;
    let got = post(&rig, &owner("slack:home/U1"), &card()).await;
    let opened = open_calls(&rig);
    let posted = card_posts(&rig);
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].body["users"], "U1");
    assert_eq!(opened[0].token, "test-bot-1");
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].body["channel"], "D1");
    assert!(posted[0].body.get("thread_ts").is_none());
    let blocks = posted[0].body["blocks"].as_array().unwrap();
    assert_eq!(blocks[0]["text"]["text"], "Scheduled delivery approval");
    assert_eq!(
        blocks[1]["text"]["text"],
        "Recipient: Team channel\nInstruction: Notify when data arrives"
    );
    let buttons = blocks.last().unwrap()["elements"].as_array().unwrap();
    assert!(buttons.iter().all(|b| b["value"] == "n-1"));
    assert_eq!(
        posted[0].body["text"],
        "Scheduled delivery approval\nRecipient: Team channel\nInstruction: Notify when data arrives"
    );
    let answered = posted[0].posted().unwrap();
    assert_eq!(
        got,
        CardPost::Posted {
            team: "T1".into(),
            dm: answered.0,
            ts: answered.1
        }
    );
    assert!(matches!(&got, CardPost::Posted { dm, .. } if dm == "D1"));
}

#[tokio::test]
async fn an_owner_who_is_not_a_user_of_this_installation_gets_no_dm_and_no_slack_call() {
    let rig = fresh().await;
    for who in [
        "slack:other/U1",
        "local:owner",
        "http:inbox/U1",
        "slack:home/u1",
        "slack:home/",
        "slack:home/U1/extra",
        "slack:home",
        "U1",
        "",
    ] {
        assert_eq!(
            post(&rig, &owner(who), &card()).await,
            CardPost::Unavailable,
            "{who:?}"
        );
    }
    assert!(
        rig.slack.all_calls().is_empty(),
        "must not call Slack when DM recipient cannot be resolved"
    );
    assert_eq!(slack_user(&owner("slack:home/U1")), Some(("home", "U1")));
}

#[tokio::test]
async fn a_card_that_does_not_fit_is_never_replaced_by_a_notice_or_a_cut_card() {
    let rig = fresh().await;
    let long = Card {
        nonce: "n-2".into(),
        title: "Large change".into(),
        lines: vec!["€".repeat(500); 7],
    };
    assert_eq!(
        post(&rig, &owner("slack:home/U1"), &long).await,
        CardPost::Unavailable
    );
    assert!(
        rig.slack.all_calls().is_empty(),
        "must neither open DM nor post an oversized card"
    );
}

#[tokio::test]
async fn a_dm_that_cannot_be_opened_is_unavailable_and_nothing_is_posted() {
    for code in [
        "user_not_found",
        "user_not_visible",
        "cannot_dm_bot",
        "missing_scope",
        "not_allowed_token_type",
        "users_list_not_supplied",
        "channel_not_found",
    ] {
        let rig = fresh().await;
        rig.slack.script(
            "conversations.open",
            Scripted::Json(json!({"ok": false, "error": code})),
        );
        let got = post(&rig, &owner("slack:home/U1"), &card()).await;
        assert_eq!(got, CardPost::Unavailable, "{code}");
        assert!(card_posts(&rig).is_empty(), "{code}");
    }
    // Non-DM channel IDs or malformed open responses must never receive the card.
    for reply in [
        json!({"ok": true}),
        json!({"ok": true, "channel": {}}),
        json!({"ok": true, "channel": {"id": "C1"}}),
        json!({"ok": true, "channel": {"id": "G1"}}),
        json!({"ok": true, "channel": {"id": 7}}),
        json!({"ok": true, "channel": {"id": "d1"}}),
        json!({"ok": true, "channel": "D1"}),
    ] {
        let rig = fresh().await;
        rig.slack
            .script("conversations.open", Scripted::Json(reply.clone()));
        assert_eq!(
            post(&rig, &owner("slack:home/U1"), &card()).await,
            CardPost::Unavailable,
            "{reply}"
        );
        assert!(card_posts(&rig).is_empty(), "{reply}");
    }
}

#[tokio::test]
async fn a_transient_open_failure_posts_nothing_and_asks_to_try_again() {
    let transient = |reason, retry_after| CardPost::NotSent {
        reason,
        retry_after,
    };
    let rig = fresh().await;
    rig.slack.script(
        "conversations.open",
        Scripted::Raw {
            status: 429,
            retry_after: Some(7),
            body: String::new(),
        },
    );
    assert_eq!(
        post(&rig, &owner("slack:home/U1"), &card()).await,
        transient(NotSentReason::RateLimited, Some(Duration::from_secs(7)))
    );
    assert!(card_posts(&rig).is_empty());
    // `conversations.open` is idempotent, so 5xx/server errors confirm nothing was posted.
    for scripted in [
        Scripted::Json(json!({"ok": false, "error": "internal_error"})),
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: String::new(),
        },
        Scripted::Raw {
            status: 200,
            retry_after: None,
            body: "not json".into(),
        },
    ] {
        let rig = fresh().await;
        rig.slack.script("conversations.open", scripted);
        assert_eq!(
            post(&rig, &owner("slack:home/U1"), &card()).await,
            transient(NotSentReason::BeforeWrite, None)
        );
        assert!(card_posts(&rig).is_empty());
    }
    let rig = fresh().await;
    rig.slack.script(
        "conversations.open",
        Scripted::Json(json!({"ok": false, "error": "internal_error"})),
    );
    assert!(matches!(
        post(&rig, &owner("slack:home/U1"), &card()).await,
        CardPost::NotSent { .. }
    ));
    assert!(matches!(
        post(&rig, &owner("slack:home/U1"), &card()).await,
        CardPost::Posted { .. }
    ));
    assert_eq!(open_calls(&rig).len(), 2);
    assert_eq!(card_posts(&rig).len(), 1);
}

#[tokio::test]
async fn a_credential_that_cannot_be_read_or_checked_sends_nothing() {
    let rig = rig_with(
        support::personal(),
        support::secrets(&[("personal", "app-token", "x")]),
        None,
    )
    .await;
    assert_eq!(
        post(&rig, &owner("slack:home/U1"), &card()).await,
        CardPost::NotSent {
            reason: NotSentReason::BeforeWrite,
            retry_after: None
        }
    );
    assert!(rig.slack.all_calls().is_empty());
    // Token belonging to another team must not open a DM.
    let rig = fresh().await;
    rig.slack.set_auth("T9", "UBOT");
    assert!(matches!(
        post(&rig, &owner("slack:home/U1"), &card()).await,
        CardPost::NotSent { .. }
    ));
    assert!(open_calls(&rig).is_empty() && card_posts(&rig).is_empty());
}

async fn posted_with(reply: Scripted) -> (CardPost, usize) {
    let rig = fresh().await;
    rig.slack.script("chat.postMessage", reply);
    let got = post(&rig, &owner("slack:home/U1"), &card()).await;
    (got, card_posts(&rig).len())
}

#[tokio::test]
async fn a_post_that_slack_refused_for_a_reason_is_unavailable_and_an_unclear_one_is_held_without_reposting()
 {
    // Explicit Slack rejections never downgrade an approval card to plain text.
    for code in [
        "channel_not_found",
        "is_archived",
        "msg_too_long",
        "invalid_blocks",
        "invalid_blocks_format",
        "msg_blocks_too_long",
        "not_in_channel",
    ] {
        let (got, posts) = posted_with(Scripted::Json(json!({"ok": false, "error": code}))).await;
        assert_eq!((got, posts), (CardPost::Unavailable, 1), "{code}");
    }
    let (got, posts) = posted_with(Scripted::Raw {
        status: 429,
        retry_after: Some(3),
        body: String::new(),
    })
    .await;
    assert_eq!(
        (got, posts),
        (
            CardPost::NotSent {
                reason: NotSentReason::RateLimited,
                retry_after: Some(Duration::from_secs(3))
            },
            1
        )
    );
    // Ambiguous post responses hold without auto-reposting.
    let unclear = [
        Scripted::Json(json!({"ok": false, "error": "internal_error"})),
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: String::new(),
        },
        Scripted::Raw {
            status: 200,
            retry_after: None,
            body: "not json".into(),
        },
        Scripted::Json(json!({"ok": true, "channel": "D2", "ts": "1700000000.000009"})),
        Scripted::Json(json!({"ok": true, "channel": "C1", "ts": "1700000000.000009"})),
        Scripted::Json(json!({"ok": true, "channel": "D1"})),
        Scripted::Json(json!({"ok": true, "ts": "1700000000.000009"})),
        Scripted::Json(json!({"ok": true, "channel": "D1", "ts": "later"})),
        Scripted::Json(
            json!({"ok": true, "channel": "D1", "ts": "1700000000.000009",
            "response_metadata": {"warnings": ["message_truncated"]}}),
        ),
    ];
    for reply in unclear {
        let shown: Value = match &reply {
            Scripted::Json(value) => value.clone(),
            Scripted::Raw { status, body, .. } => json!({"http": status, "body": body}),
            Scripted::Delayed { body, .. } => body.clone(),
        };
        let (got, posts) = posted_with(reply).await;
        assert_eq!((got, posts), (CardPost::Held, 1), "{shown}");
    }
}

#[tokio::test]
async fn each_owner_gets_only_their_own_dm_and_the_card_never_goes_to_the_channel_it_came_from() {
    let rig = fresh().await;
    let first = post(&rig, &owner("slack:home/U1"), &card()).await;
    let second = post(&rig, &owner("slack:home/U2"), &card()).await;
    assert!(matches!(&first, CardPost::Posted { dm, .. } if dm == "D1"));
    assert!(matches!(&second, CardPost::Posted { dm, .. } if dm == "D2"));
    let users: Vec<_> = open_calls(&rig)
        .iter()
        .map(|call| call.body["users"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(users, ["U1", "U2"]);
    let channels: Vec<_> = card_posts(&rig)
        .iter()
        .map(|call| call.body["channel"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(channels, ["D1", "D2"], "must not post to channels");
}

#[tokio::test]
async fn channel_origin_dm_card_includes_a_validated_thread_link_using_workspace_url_or_fallback() {
    let rig = fresh().await;
    rig.slack.set_url(Some("https://acme.slack.com/"));
    let reply_source = CardSource {
        channel: "C2".into(),
        thread_ts: Some("1700000000.000100".into()),
        message_ts: Some("1700000000.000250".into()),
    };
    let sender = rig.sender();
    let got = sender
        .post_owner_card(
            &personal(),
            "home",
            &owner("slack:home/U1"),
            &card(),
            Some(&reply_source),
        )
        .await;
    assert!(matches!(got, CardPost::Posted { .. }));
    let sent = &card_posts(&rig)[0];
    let expected_url =
        "https://acme.slack.com/archives/C2/p1700000000000250?thread_ts=1700000000.000100&cid=C2";
    let blocks = sent.body["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 4);
    assert_eq!(blocks[0]["type"], "header");
    assert_eq!(blocks[1]["type"], "context");
    assert_eq!(
        blocks[1]["elements"][0]["text"],
        format!("Requested in <#C2> · <{expected_url}|View thread>")
    );
    assert_eq!(blocks[2]["text"]["type"], "plain_text");
    assert_eq!(blocks[3]["type"], "actions");
    assert!(
        sent.body["text"]
            .as_str()
            .unwrap()
            .ends_with(&format!("\nThread: {expected_url}")),
        "{}",
        sent.body["text"]
    );
    assert_eq!(sent.body["unfurl_links"], false);

    // Without auth.test `url`, thread root links fall back cleanly to `https://slack.com/`.
    let rig = fresh().await;
    let root_source = CardSource {
        channel: "C2".into(),
        thread_ts: Some("1700000000.000100".into()),
        message_ts: None,
    };
    let sender = rig.sender();
    assert!(matches!(
        sender
            .post_owner_card(
                &personal(),
                "home",
                &owner("slack:home/U1"),
                &card(),
                Some(&root_source),
            )
            .await,
        CardPost::Posted { .. }
    ));
    let sent = &card_posts(&rig)[0];
    assert_eq!(
        sent.body["blocks"][1]["elements"][0]["text"],
        "Requested in <#C2> · <https://slack.com/archives/C2/p1700000000000100|View thread>"
    );
}

#[tokio::test]
async fn configured_slack_dm_sent_notice_overrides_the_channel_notice_copy() {
    use bot_core::{
        cards::DM_SENT_NOTICE,
        state::{Conversation, ConversationKind},
    };
    use bot_gateway::{
        delivery::{Delivery, Sender},
        reply::ReplyBody,
        testing::ports::outgoing,
    };

    let host = r#"{"slack":{"dm_sent_notice":"Approval sent via DM (custom)","workspaces":{"home":{
        "team_id":"T1","dm":true,"bot_token":"bot-token","app_token":"app-token",
        "channels":{"planning":"C1"}}}}}"#;
    let rig = rig_on(host, support::personal(), support::default_secrets(), None).await;
    let snap = support::personal().build();
    let item = outgoing(
        "personal",
        0,
        "team",
        "slack:home/U1",
        Conversation {
            kind: ConversationKind::Channel,
            id: "C1".into(),
            thread: Some("1700000000.000100".into()),
        },
        &json!({"text": DM_SENT_NOTICE}).to_string(),
    );
    let body = ReplyBody {
        text: Some(DM_SENT_NOTICE.into()),
        attachments: vec![],
        approval: None,
    };
    let sent = rig.sender().send(&snap, &item, &body).await;
    assert!(matches!(sent, Delivery::Sent { .. }), "{sent:?}");
    let posts = card_posts(&rig);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["text"], "Approval sent via DM (custom)");
    assert_eq!(
        posts[0].body["blocks"][0]["text"],
        "Approval sent via DM (custom)"
    );
}
