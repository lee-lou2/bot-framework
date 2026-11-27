//! Slack outbound correctness tests: rotated token verification, lossless body limits, receipt confirmation, and thread evidence.
use crate::delivery_support::*;
use crate::support;
use crate::support::default_secrets;
use bot_core::ledger::ApprovalState;
use bot_gateway::testing::{
    ports::outgoing,
    slack::{Scripted, button_frame},
};
use serde_json::{Value, json};
use std::time::Duration;

const TTL: Duration = Duration::from_millis(40);

async fn wait_out_the_cache() {
    tokio::time::sleep(TTL * 2).await;
}

async fn fresh() -> Rig {
    rig().await
}

fn tokens(rig: &Rig) -> Vec<String> {
    posts(rig).into_iter().map(|call| call.token).collect()
}

#[tokio::test]
async fn a_rotated_bot_token_posts_only_after_it_is_checked_against_the_registered_team() {
    let secrets = default_secrets();
    let rig = rig_ttl(secrets.clone(), TTL).await;
    to_channel(&rig, 1, json!({"text": "one"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(tokens(&rig), ["test-bot-1"]);
    assert_eq!(
        rig.slack.calls("auth.test").len(),
        1,
        "first use verifies the token"
    );

    wait_out_the_cache().await;
    to_channel(&rig, 2, json!({"text": "two"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(tokens(&rig), ["test-bot-1", "test-bot-1"]);
    assert_eq!(rig.slack.calls("auth.test").len(), 1);

    // Token swapped to another team must not post.
    secrets.set("personal", "bot-token", b"other-team-token");
    rig.slack.set_auth("T9", "UBOT");
    wait_out_the_cache().await;
    to_channel(&rig, 3, json!({"text": "three"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(
        posts(&rig).len(),
        2,
        "must not post with foreign team token"
    );
    assert_eq!(settled(&rig).last().unwrap(), "notsent:BeforeWrite:0");
    let checks = rig.slack.calls("auth.test");
    assert_eq!(
        (checks.len(), checks[1].token.as_str()),
        (2, "other-team-token")
    );

    secrets.set("personal", "bot-token", b"unchecked-token");
    rig.slack.script(
        "auth.test",
        Scripted::Json(json!({"ok": false, "error": "internal_error"})),
    );
    wait_out_the_cache().await;
    to_channel(&rig, 4, json!({"text": "four"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 2);
    assert_eq!(settled(&rig).last().unwrap(), "notsent:BeforeWrite:0");

    secrets.set("personal", "bot-token", b"rotated-token");
    rig.slack.set_auth("T1", "UBOT");
    wait_out_the_cache().await;
    to_channel(&rig, 5, json!({"text": "five"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(tokens(&rig), ["test-bot-1", "test-bot-1", "rotated-token"]);
    assert_eq!(rig.slack.calls("auth.test").len(), 4);
}

#[tokio::test]
async fn a_body_is_sent_whole_or_not_completed() {
    // Plain-text fallback after invalid_blocks must preserve the full body beyond 3,000 chars.
    let rig = fresh().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_blocks"})),
    );
    let text = "€".repeat(3_104);
    to_channel(&rig, 1, json!({ "text": text }));
    rig.deliverer.tick().await.unwrap();
    let calls = posts(&rig);
    assert_eq!(calls.len(), 2);
    assert!(calls[1].body.get("blocks").is_none());
    assert_eq!(calls[1].body["text"].as_str().unwrap(), text);
    assert!(settled(&rig)[0].starts_with("confirmed:C1:"));

    // Bodies up to the 11,000-char markdown block limit fit in one block.
    let rig = fresh().await;
    let text = "★".repeat(11_000);
    to_channel(&rig, 1, json!({ "text": text }));
    rig.deliverer.tick().await.unwrap();
    let calls = posts(&rig);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["blocks"][0]["text"].as_str().unwrap(), text);

    // Bodies above the block limit (within the 12,000-char reply limit) go as plain text without truncation.
    let rig = fresh().await;
    let text = "€".repeat(11_800);
    to_channel(&rig, 1, json!({ "text": text }));
    rig.deliverer.tick().await.unwrap();
    let calls = posts(&rig);
    assert_eq!(calls.len(), 1);
    assert!(calls[0].body.get("blocks").is_none());
    assert_eq!(calls[0].body["text"].as_str().unwrap(), text);
    assert!(settled(&rig)[0].starts_with("confirmed:C1:"));

    // Neutralizing broadcast mentions expands length past the block limit and falls back to plain text.
    let rig = fresh().await;
    to_channel(&rig, 1, json!({ "text": "<!here>".repeat(1_700) }));
    rig.deliverer.tick().await.unwrap();
    let calls = posts(&rig);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].body["text"].as_str().unwrap(),
        "&lt;!here>".repeat(1_700)
    );

    // Oversized body paired with an approval card is rejected without calling Slack.
    let rig = fresh().await;
    to_channel(
        &rig,
        1,
        json!({"text": "★".repeat(11_800), "approval": {"nonce": "n", "title": "t", "lines": ["x"]}}),
    );
    rig.deliverer.tick().await.unwrap();
    assert!(rig.slack.all_calls().is_empty());
    assert_eq!(settled(&rig), ["rejected"]);
}

async fn settled_for(reply: Value) -> String {
    let rig = fresh().await;
    rig.slack.script("chat.postMessage", Scripted::Json(reply));
    to_channel(&rig, 1, json!({"text": "hello"}));
    rig.deliverer.tick().await.unwrap();
    settled(&rig).remove(0)
}

#[tokio::test]
async fn a_receipt_is_only_what_the_slack_response_confirms() {
    for reply in [
        json!({"ok": true, "channel": "C2", "ts": "1700000000.000009"}),
        json!({"ok": true, "ts": "1700000000.000009"}),
        json!({"ok": true, "channel": "C1"}),
        json!({"ok": true, "channel": "C1", "ts": "later"}),
        json!({"ok": true, "channel": ["C1"], "ts": "1700000000.000009"}),
    ] {
        assert_eq!(settled_for(reply.clone()).await, "held", "{reply}");
    }
    assert_eq!(
        settled_for(json!({"ok": true, "channel": "C1", "ts": "1700000000.000009"})).await,
        "confirmed:C1:1700000000.000009"
    );

    // `message_truncated` holds without auto-resend; unrelated warnings still confirm.
    let truncated = json!({"ok": true, "channel": "C1", "ts": "1700000000.000009",
        "response_metadata": {"warnings": ["missing_charset", "message_truncated"]}});
    assert_eq!(settled_for(truncated).await, "held");
    let unrelated = json!({"ok": true, "channel": "C1", "ts": "1700000000.000009",
        "warning": "missing_charset", "response_metadata": {"warnings": ["missing_charset"]}});
    assert_eq!(
        settled_for(unrelated).await,
        "confirmed:C1:1700000000.000009"
    );
    let rig = fresh().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(
            json!({"ok": true, "channel": "C1", "ts": "1700000000.000009",
            "response_metadata": {"warnings": ["message_truncated"]}}),
        ),
    );
    to_channel(&rig, 1, json!({"text": "hello"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 1);
    assert_eq!(rig.deliverer.tick().await.unwrap(), 0);
    assert_eq!(posts(&rig).len(), 1);

    let rig = fresh().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_blocks"})),
    );
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": true, "channel": "C2", "ts": "1700000000.000009"})),
    );
    to_channel(&rig, 1, json!({"text": "hello"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["held"]);

    // Upload completion must confirm the exact set of uploaded file IDs.
    for reply in [
        json!({"ok": true}),
        json!({"ok": true, "files": []}),
        json!({"ok": true, "files": [{"id": "FOTHER"}]}),
        json!({"ok": true, "files": [{"id": "FAAA1"}, {"id": "FOTHER"}]}),
        json!({"ok": true, "files": [{"id": "FAAA1"}, {"title": "no id"}]}),
        json!({"ok": true, "files": [{"id": "FAAA1"}, {"id": ""}]}),
        json!({"ok": true, "files": [{"id": ""}]}),
        json!({"ok": true, "files": [{"id": 7}]}),
        json!({"ok": true, "files": [{"id": "FAAA1"}, {"id": "FAAA1"}]}),
    ] {
        let rig = uploading(&["FAAA1"]).await;
        rig.slack.script(
            "files.completeUploadExternal",
            Scripted::Json(reply.clone()),
        );
        to_channel(&rig, 1, one_attachment());
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig), ["held"], "{reply}");
    }
    let rig = uploading(&["FAAA1"]).await;
    rig.slack.script(
        "files.completeUploadExternal",
        Scripted::Json(json!({"ok": true, "files": [{"id": "FAAA1", "title": "a.txt"}]})),
    );
    to_channel(&rig, 1, one_attachment());
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["confirmed:C1:FAAA1"]);

    // Malformed upload slot responses reject before uploading or publishing.
    for slot in [
        json!({"ok": true, "upload_url": "@URL@"}),
        json!({"ok": true, "upload_url": "@URL@", "file_id": 7}),
        json!({"ok": true, "upload_url": "@URL@", "file_id": ""}),
        json!({"ok": true, "upload_url": "@URL@", "file_id": null}),
        json!({"ok": true, "file_id": "FAAA1"}),
        json!({"ok": true, "upload_url": 5, "file_id": "FAAA1"}),
        json!({"ok": true, "upload_url": "", "file_id": "FAAA1"}),
        json!({"ok": true}),
    ] {
        let rig = fresh().await;
        let url = format!("{}upload/0", rig.slack.base.trim_end_matches("api/"));
        let slot = serde_json::from_str::<Value>(&slot.to_string().replace("@URL@", &url)).unwrap();
        rig.slack
            .script("files.getUploadURLExternal", Scripted::Json(slot.clone()));
        to_channel(&rig, 1, one_attachment());
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig), ["rejected"], "{slot}");
        assert!(rig.slack.uploads().is_empty(), "{slot}");
        assert!(
            rig.slack.calls("files.completeUploadExternal").is_empty(),
            "{slot}"
        );
    }
    let rig = fresh().await;
    rig.slack.script(
        "files.getUploadURLExternal",
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: String::new(),
        },
    );
    to_channel(&rig, 1, one_attachment());
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["held"]);
    assert!(rig.slack.calls("files.completeUploadExternal").is_empty());

    // Empty or duplicate upload file IDs reject before completion.
    for ids in [vec![""], vec!["FAAA1", "FAAA1"]] {
        let rig = uploading(&ids).await;
        to_channel(&rig, 1, two_attachments());
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig), ["rejected"], "{ids:?}");
        assert!(
            rig.slack.calls("files.completeUploadExternal").is_empty(),
            "{ids:?}"
        );
    }
}

fn one_attachment() -> Value {
    json!({"text": "report", "attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]})
}

fn two_attachments() -> Value {
    json!({"text": "report", "attachments": [
        {"name": "a.txt", "type": "text/plain", "data": "aGk="},
        {"name": "b.txt", "type": "text/plain", "data": "aGk="}]})
}

async fn uploading(ids: &[&str]) -> Rig {
    let rig = fresh().await;
    let base = rig.slack.base.trim_end_matches("api/").to_owned();
    for (n, id) in ids.iter().enumerate() {
        rig.slack.script(
            "files.getUploadURLExternal",
            Scripted::Json(
                json!({"ok": true, "upload_url": format!("{base}upload/{n}"), "file_id": id}),
            ),
        );
    }
    rig
}

#[tokio::test]
async fn the_fake_slack_answers_like_slack_so_ordinary_sends_are_confirmed() {
    let rig = fresh().await;
    to_channel(&rig, 1, json!({"text": "ok"}));
    rig.outbox.push(outgoing(
        "personal",
        2,
        "direct",
        "slack:home/U1",
        direct("D1"),
        &json!({"text": "DM"}).to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    let all = settled(&rig);
    assert!(all.iter().all(|s| s.starts_with("confirmed:")), "{all:?}");
}

#[tokio::test]
async fn a_card_refresh_goes_through_the_same_token_check_as_sending() {
    let secrets = default_secrets();
    let rig = support::rig_full(
        vec![support::personal()],
        secrets.clone(),
        support::host(""),
        None,
        Some(TTL),
        |_| {},
    )
    .await;
    assert!(rig.slack.connected().await);
    let card =
        json!({"nonce": "n1", "title": "Delete event", "lines": ["Target: Walk"]}).to_string();
    rig.ledger
        .put("n1", "personal", ApprovalState::Approved, &card);
    secrets.set("personal", "bot-token", b"other-team-token");
    rig.slack.set_auth("T9", "UBOT");
    wait_out_the_cache().await;
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
    for _ in 0..100 {
        if !rig.broker.decisions().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rig.broker.decisions().len(), 1, "decision is recorded");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rig.slack.calls("chat.update").is_empty());
    assert!(rig.slack.calls("chat.postEphemeral").is_empty());
    assert!(
        rig.slack
            .calls("auth.test")
            .iter()
            .any(|call| call.token == "other-team-token"),
        "new token was verified"
    );
}

#[tokio::test]
async fn the_top_level_text_carries_the_whole_body_and_the_approval_it_asks_for() {
    let card = json!({"nonce": "n1", "title": "Delete event", "lines": ["Target: Walk", "Irreversible action"]});
    let rig = fresh().await;
    let body = "€".repeat(3_100);
    to_channel(&rig, 1, json!({"text": body, "approval": card}));
    rig.deliverer.tick().await.unwrap();
    let sent = posts(&rig);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].body["text"].as_str().unwrap(),
        format!("{body}\n\nDelete event\nTarget: Walk\nIrreversible action")
    );
    assert_eq!(sent[0].body["blocks"][0]["text"].as_str().unwrap(), body);
    assert!(
        sent[0].body["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "actions")
    );

    let rig = fresh().await;
    to_channel(
        &rig,
        1,
        json!({"text": "<!channel> please check", "approval": card}),
    );
    rig.deliverer.tick().await.unwrap();
    let text = posts(&rig)[0].body["text"].as_str().unwrap().to_owned();
    assert!(text.starts_with("&lt;!channel> please check\n\n"), "{text}");
    assert!(!text.contains("<!"), "{text}");

    let rig = fresh().await;
    to_channel(&rig, 1, json!({"approval": card}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(
        posts(&rig)[0].body["text"].as_str().unwrap(),
        "Delete event\nTarget: Walk\nIrreversible action"
    );

    // Oversized card appends an admin UI approval notice after the full body.
    let rig = fresh().await;
    let long = json!({"nonce": "n2", "title": "Large change", "lines": vec!["€".repeat(500); 7]});
    to_channel(&rig, 1, json!({"text": body, "approval": long}));
    rig.deliverer.tick().await.unwrap();
    let text = posts(&rig)[0].body["text"].as_str().unwrap().to_owned();
    assert!(text.starts_with(&body), "full body comes first");
    let tail: String = text.chars().skip(3_100).collect();
    assert!(
        tail.contains("https://admin.example/approvals")
            && tail.to_ascii_lowercase().contains("approv"),
        "{tail}"
    );
}

#[tokio::test]
async fn the_test_helper_counts_a_post_only_when_the_sender_would_confirm_it() {
    use bot_gateway::slack::api::{Body, SlackApi};
    let slack = bot_gateway::testing::slack::FakeSlack::start().await;
    let api = SlackApi::new(Some(&slack.base));
    let ts = "1700000000.000009";
    let cases = [
        (
            Scripted::Json(json!({"ok": true, "channel": "D1", "ts": ts})),
            true,
        ),
        (
            Scripted::Json(json!({"ok": true, "channel": "D2", "ts": ts})),
            false,
        ),
        (Scripted::Json(json!({"ok": true, "channel": "D1"})), false),
        (
            Scripted::Json(json!({"ok": true, "channel": "D1", "ts": "later"})),
            false,
        ),
        (
            Scripted::Json(json!({"ok": true, "channel": "D1", "ts": ts,
                "response_metadata": {"warnings": ["message_truncated"]}})),
            false,
        ),
        (
            Scripted::Json(json!({"ok": false, "error": "channel_not_found"})),
            false,
        ),
        (
            Scripted::Raw {
                status: 502,
                retry_after: None,
                body: json!({"ok": true, "channel": "D1", "ts": ts}).to_string(),
            },
            false,
        ),
    ];
    for (scripted, confirmed) in cases {
        slack.script("chat.postMessage", scripted);
        let message = json!({"channel": "D1", "text": "card"});
        let _ = api
            .call("chat.postMessage", "token", Body::Json(&message))
            .await;
        let call = slack.calls("chat.postMessage").pop().unwrap();
        assert_eq!(
            call.posted(),
            confirmed.then(|| ("D1".to_owned(), ts.to_owned())),
            "{:?} {}",
            call.reply,
            call.status
        );
    }
    assert!(!slack.calls("chat.postMessage").last().unwrap().succeeded());
}

#[tokio::test]
async fn a_confirmed_channel_post_is_recorded_as_thread_evidence() {
    let rig = fresh().await;
    to_channel(&rig, 1, json!({"text": "reply"}));
    rig.deliverer.tick().await.unwrap();
    let notes = rig.threads.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    let n = &notes[0];
    assert_eq!(
        (
            n.profile.0.as_str(),
            n.binding.0.as_str(),
            n.team.as_str(),
            n.bot.as_str(),
            n.channel.as_str(),
            n.root_ts.as_str()
        ),
        ("personal", "team", "T1", "UBOT", "C1", THREAD)
    );

    // Top-level channel post records its own ts as the thread root.
    let mut top = outgoing(
        "personal",
        2,
        "team",
        "slack:home/U1",
        bot_core::state::Conversation {
            kind: bot_core::state::ConversationKind::Channel,
            id: "C1".into(),
            thread: None,
        },
        &json!({"text": "announcement"}).to_string(),
    );
    top.call = bot_core::id::CallId("reply-2".into());
    rig.outbox.push(top);
    rig.deliverer.tick().await.unwrap();
    let roots: Vec<String> = rig.threads.notes().into_iter().map(|n| n.root_ts).collect();
    assert_eq!(roots.len(), 2);
    assert_eq!(roots[0], THREAD);
    assert_ne!(roots[1], THREAD, "{roots:?}");
    assert!(roots[1].contains('.'), "{roots:?}");

    // DMs and rejected channel posts are not recorded as thread evidence.
    rig.outbox.push(outgoing(
        "personal",
        3,
        "direct",
        "slack:home/U1",
        direct("D1"),
        &json!({"text": "DM"}).to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "channel_not_found"})),
    );
    to_channel(&rig, 4, json!({"text": "rejected post"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(rig.threads.notes().len(), 2);
}

#[tokio::test]
async fn a_failed_evidence_write_does_not_change_the_delivery_outcome() {
    let rig = fresh().await;
    rig.threads.fail_writes(true);
    to_channel(&rig, 1, json!({"text": "reply"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 1);
    assert!(
        settled(&rig)[0].starts_with("confirmed"),
        "{:?}",
        settled(&rig)
    );
    assert!(rig.threads.notes().is_empty());
}
