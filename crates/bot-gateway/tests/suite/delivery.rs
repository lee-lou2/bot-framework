//! Reply delivery acceptance tests: outbox claim, current revision check, begin, single Slack send, and settle.
use bot_core::{
    id::{ProfileId, RunId},
    intake::PushTransportId,
    port::Gateway,
    state::{Conversation, ConversationKind},
};
use bot_gateway::testing::{ports::outgoing, slack::Scripted, *};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::delivery_support::*;
use crate::support::{default_secrets, personal, secrets};

#[tokio::test]
async fn a_text_reply_goes_to_the_original_thread_once_and_is_confirmed() {
    let rig = rig().await;
    to_channel(&rig, 1, json!({"text": "You have 3 events"}));
    assert_eq!(rig.deliverer.tick().await.unwrap(), 1);
    let calls = posts(&rig);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].token, "test-bot-1");
    let body = &calls[0].body;
    assert_eq!(
        (body["channel"].as_str(), body["thread_ts"].as_str()),
        (Some("C1"), Some(THREAD))
    );
    assert_eq!(body["blocks"][0]["type"], "markdown");
    assert_eq!(body["blocks"][0]["text"], "You have 3 events");
    assert_eq!(body["text"], "You have 3 events");
    assert_eq!(settled(&rig), ["confirmed:C1:1700000000.000002"]);
    assert_eq!(rig.outbox.begun()[0].1, "personal-r1");
    assert_eq!(rig.deliverer.tick().await.unwrap(), 0);
    assert_eq!(posts(&rig).len(), 1);
}

#[tokio::test]
async fn dm_replies_use_the_dm_channel_and_broadcast_mentions_are_neutralised() {
    let rig = rig().await;
    rig.outbox.push(outgoing(
        "personal",
        1,
        "direct",
        "slack:home/U1",
        direct("D1"),
        &json!({"text": "<!channel> hello"}).to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    let body = &posts(&rig)[0].body;
    assert_eq!(body["channel"], "D1");
    assert_eq!(body["blocks"][0]["text"], "&lt;!channel> hello");
    assert_eq!(body["text"], "&lt;!channel> hello");
}

#[tokio::test]
async fn approval_cards_carry_buttons_and_a_card_that_would_be_cut_does_not() {
    let rig = rig().await;
    let card = json!({"nonce": "n-1", "title": "Delete event", "lines": ["Target: Walk"]});
    to_channel(
        &rig,
        1,
        json!({"text": "Approval required", "approval": card}),
    );
    rig.deliverer.tick().await.unwrap();
    let blocks = posts(&rig)[0].body["blocks"].as_array().unwrap().clone();
    let types: Vec<_> = blocks.iter().map(|b| b["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["markdown", "header", "section", "actions"]);
    let buttons = blocks[3]["elements"].as_array().unwrap();
    assert_eq!(buttons[0]["action_id"], "bot_approve");
    assert_eq!(buttons[1]["action_id"], "bot_deny");
    assert!(buttons.iter().all(|b| b["value"] == "n-1"));

    // Oversized cards omit action buttons and direct approvers to the admin UI.
    let long = json!({"nonce": "n-2", "title": "Large change", "lines": vec!["€".repeat(500); 7]});
    to_channel(&rig, 2, json!({"approval": long}));
    rig.deliverer.tick().await.unwrap();
    let second = &posts(&rig)[1].body;
    assert!(
        second["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["type"] != "actions")
    );
    assert!(
        second["text"]
            .as_str()
            .unwrap()
            .contains("https://admin.example/approvals")
    );
    assert_eq!(settled(&rig).len(), 2);
}

#[tokio::test]
async fn outcomes_are_settled_by_what_slack_proved_and_nothing_is_resent() {
    for (script, want) in [
        (
            Scripted::Raw {
                status: 429,
                retry_after: Some(7),
                body: String::new(),
            },
            "notsent:RateLimited:7",
        ),
        (
            Scripted::Json(json!({"ok": false, "error": "channel_not_found"})),
            "rejected",
        ),
        (
            Scripted::Json(json!({"ok": false, "error": "internal_error"})),
            "held",
        ),
        (
            Scripted::Raw {
                status: 500,
                retry_after: None,
                body: "oops".into(),
            },
            "held",
        ),
        (
            Scripted::Raw {
                status: 502,
                retry_after: None,
                body: r#"{"ok":true,"ts":"1.1"}"#.into(),
            },
            "held",
        ),
        (Scripted::Json(json!({"ok": true})), "held"),
    ] {
        let rig = rig().await;
        rig.slack.script("chat.postMessage", script);
        to_channel(&rig, 1, json!({"text": "hello"}));
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig), [want]);
        assert_eq!(posts(&rig).len(), 1, "{want}: must not silently resend");
    }
}

#[tokio::test]
async fn a_refused_connection_is_not_sent() {
    let rig = rig_with(
        personal(),
        default_secrets(),
        Some("http://127.0.0.1:1/api/"),
    )
    .await;
    to_channel(&rig, 1, json!({"text": "hello"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["notsent:BeforeWrite:0"]);
}

#[tokio::test]
async fn a_missing_bot_token_sends_nothing() {
    let rig = rig_with(personal(), secrets(&[("personal", "app-token", "x")]), None).await;
    to_channel(&rig, 1, json!({"text": "hello"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["notsent:BeforeWrite:0"]);
    assert!(rig.slack.all_calls().is_empty());
}

#[tokio::test]
async fn a_format_rejection_falls_back_to_plain_text_once_but_never_for_cards() {
    let rig = rig().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_blocks"})),
    );
    to_channel(&rig, 1, json!({"text": "broken table"}));
    rig.deliverer.tick().await.unwrap();
    let calls = posts(&rig);
    assert_eq!(calls.len(), 2);
    assert!(calls[0].body.get("blocks").is_some());
    assert!(calls[1].body.get("blocks").is_none());
    assert_eq!(calls[1].body["text"], "broken table");
    assert_eq!(settled(&rig), ["confirmed:C1:1700000000.000002"]);

    // Approval cards require buttons, so format rejections do not fall back to plain text.
    let rig = self::rig().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_blocks"})),
    );
    to_channel(
        &rig,
        1,
        json!({"approval": {"nonce": "n", "title": "t", "lines": ["x"]}}),
    );
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 1);
    assert_eq!(settled(&rig), ["rejected"]);
}

#[tokio::test]
async fn replies_outside_the_original_conversation_are_rejected_before_any_slack_call() {
    let rig = rig().await;
    let ok = json!({"text": "hello"}).to_string();
    let bad_thread = Conversation {
        thread: Some("not-a-ts".into()),
        ..channel("C1")
    };
    rig.outbox.push(outgoing(
        "personal",
        1,
        "team",
        "slack:home/U1",
        channel("C9"),
        &ok,
    ));
    rig.outbox.push(outgoing(
        "personal",
        2,
        "team",
        "slack:home/U1",
        direct("D1"),
        &ok,
    ));
    rig.outbox.push(outgoing(
        "personal",
        3,
        "direct",
        "slack:home/U1",
        channel("C1"),
        &ok,
    ));
    rig.outbox.push(outgoing(
        "personal",
        4,
        "ghost",
        "slack:home/U1",
        channel("C1"),
        &ok,
    ));
    rig.outbox.push(outgoing(
        "personal",
        5,
        "team",
        "slack:home/U9",
        channel("C1"),
        &ok,
    ));
    rig.outbox.push(outgoing(
        "personal",
        6,
        "team",
        "slack:home/U1",
        bad_thread,
        &ok,
    ));
    rig.outbox.push(outgoing(
        "personal",
        7,
        "team",
        "slack:home/U1",
        channel("C1"),
        r#"{"text":"x","html":"<b>"}"#,
    ));
    rig.outbox.push(outgoing(
        "personal",
        8,
        "team",
        "slack:home/U1",
        channel("C1"),
        "not json",
    ));
    assert_eq!(rig.deliverer.tick().await.unwrap(), 8);
    assert_eq!(settled(&rig), ["rejected"; 8]);
    assert_eq!(
        rig.outbox.begun().len(),
        8,
        "rejections must still record begin before settling"
    );
    assert!(rig.slack.all_calls().is_empty(), "must not call Slack");
}

#[tokio::test]
async fn a_failed_begin_sends_nothing_and_settles_nothing() {
    let rig = rig().await;
    rig.outbox.fail_begin(true);
    to_channel(&rig, 1, json!({"text": "hello"}));
    rig.deliverer.tick().await.unwrap();
    assert!(rig.slack.all_calls().is_empty());
    assert!(settled(&rig).is_empty());
}

#[tokio::test]
async fn an_inactive_profile_is_left_alone_and_the_current_revision_is_checked() {
    let rig = rig().await;
    rig.outbox.push(outgoing(
        "ghost",
        1,
        "team",
        "slack:home/U1",
        channel("C1"),
        r#"{"text":"x"}"#,
    ));
    rig.deliverer.tick().await.unwrap();
    assert!(rig.outbox.begun().is_empty() && settled(&rig).is_empty());
    rig.catalog
        .replace([personal().revision("personal-r2").build()]);
    to_channel(&rig, 2, json!({"text": "hello"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(rig.outbox.begun()[0].1, "personal-r2");
    rig.catalog.replace([profile("personal")
        .binding(
            "team",
            bot_core::profile::Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .build()]);
    to_channel(&rig, 3, json!({"text": "again"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 1);
    assert_eq!(settled(&rig).last().map(String::as_str), Some("rejected"));
}

#[tokio::test]
async fn attachments_use_the_upload_flow_and_only_the_last_call_publishes() {
    let rig = rig().await;
    to_channel(
        &rig,
        1,
        json!({"text": "Here is the report", "attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]}),
    );
    rig.deliverer.tick().await.unwrap();
    let methods: Vec<_> = rig
        .slack
        .all_calls()
        .iter()
        .map(|c| c.method.clone())
        .collect();
    assert_eq!(
        methods,
        [
            "auth.test",
            "files.getUploadURLExternal",
            "files.completeUploadExternal"
        ]
    );
    assert_eq!(rig.slack.uploads(), [b"hi".to_vec()]);
    let slot = &rig.slack.calls("files.getUploadURLExternal")[0].body;
    assert_eq!(
        (slot["filename"].as_str(), slot["length"].as_str()),
        (Some("a.txt"), Some("2"))
    );
    let done = &rig.slack.calls("files.completeUploadExternal")[0].body;
    assert_eq!(done["channel_id"], "C1");
    assert_eq!(done["thread_ts"], THREAD);
    assert_eq!(done["initial_comment"], "Here is the report");
    assert!(
        done["files"]
            .as_str()
            .unwrap()
            .contains("\"title\":\"a.txt\"")
    );
    assert!(settled(&rig)[0].starts_with("confirmed:C1:F"));

    for (script, want) in [
        (
            Scripted::Raw {
                status: 429,
                retry_after: Some(3),
                body: String::new(),
            },
            "notsent:RateLimited:3",
        ),
        (
            Scripted::Json(json!({"ok": false, "error": "channel_not_found"})),
            "rejected",
        ),
        (
            Scripted::Raw {
                status: 500,
                retry_after: None,
                body: String::new(),
            },
            "held",
        ),
    ] {
        let rig = self::rig().await;
        rig.slack.script("files.completeUploadExternal", script);
        to_channel(
            &rig,
            1,
            json!({"attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]}),
        );
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig), [want]);
        assert_eq!(
            rig.slack.calls("files.completeUploadExternal").len(),
            1,
            "{want}: must not retry completion"
        );
    }
    let rig = self::rig().await;
    rig.slack.script(
        "files.getUploadURLExternal",
        Scripted::Raw {
            status: 429,
            retry_after: Some(5),
            body: String::new(),
        },
    );
    to_channel(
        &rig,
        1,
        json!({"attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]}),
    );
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["notsent:RateLimited:5"]);
    assert!(rig.slack.calls("files.completeUploadExternal").is_empty());
    // Approval cards and attachments cannot be combined in a single reply.
    let rig = self::rig().await;
    to_channel(
        &rig,
        1,
        json!({"attachments": [{"name": "a", "type": "a/b", "data": ""}], "approval": {"nonce": "n", "title": "t", "lines": []}}),
    );
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["rejected"]);
    assert!(rig.slack.all_calls().is_empty());
}

#[tokio::test]
async fn split_inline_attachments_post_markdown_first_and_upload_files_to_the_same_thread() {
    let rig = rig().await;
    let raw = "**Summary**.\n\nSee the attached file for details.\n\n<attachment name=\"evidence.md\">\n## Table\n| Item | Value |\n|---|---|\n| A | 1 |\n</attachment>";
    let (body, files) = bot_core::attachments::split(raw);
    let (packed, truncated) = bot_core::attachments::pack_answers(&files, 15);
    assert!(!truncated);
    assert_eq!(packed.len(), 1);

    to_channel(&rig, 1, json!({"text": body}));
    to_channel(
        &rig,
        2,
        serde_json::from_str::<serde_json::Value>(&packed[0].0).unwrap(),
    );
    assert_eq!(rig.deliverer.tick().await.unwrap(), 2);

    let posted = posts(&rig);
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].body["thread_ts"], THREAD);
    assert_eq!(posted[0].body["blocks"][0]["type"], "markdown");
    let md = posted[0].body["blocks"][0]["text"].as_str().unwrap();
    assert_eq!(md, "**Summary**.\n\nSee the attached file for details.");
    assert!(!md.contains("<attachment"));

    let slot = &rig.slack.calls("files.getUploadURLExternal")[0].body;
    let expected = "## Table\n| Item | Value |\n|---|---|\n| A | 1 |";
    assert_eq!(slot["filename"], "evidence.md");
    assert_eq!(slot["length"], expected.len().to_string());
    assert_eq!(rig.slack.uploads(), [expected.as_bytes().to_vec()]);
    let done = &rig.slack.calls("files.completeUploadExternal")[0].body;
    assert_eq!(done["channel_id"], "C1");
    assert_eq!(done["thread_ts"], THREAD);
    assert!(done.get("initial_comment").is_none());
    assert!(
        done["files"]
            .as_str()
            .unwrap()
            .contains("\"title\":\"evidence.md\"")
    );
    assert_eq!(settled(&rig).len(), 2);
    assert!(settled(&rig).iter().all(|s| s.starts_with("confirmed:C1:")));
}

#[tokio::test]
async fn slack_does_not_claim_another_transport_in_the_same_kind_of_conversation() {
    let rig = rig().await;
    let mut foreign = outgoing(
        "personal",
        1,
        "team",
        "slack:home/U1",
        Conversation {
            kind: ConversationKind::Channel,
            id: "C1".into(),
            thread: Some(THREAD.into()),
        },
        r#"{"text":"foreign transport reply"}"#,
    );
    foreign.transport = PushTransportId::new("fixture-push").unwrap();
    rig.outbox.push(foreign);
    assert_eq!(rig.deliverer.tick().await.unwrap(), 0);
    assert!(rig.outbox.begun().is_empty());
    assert!(rig.slack.all_calls().is_empty() && settled(&rig).is_empty());
}

#[tokio::test]
async fn the_run_loop_delivers_and_a_deliver_signal_wakes_it() {
    let rig = rig().await;
    let stop = CancellationToken::new();
    let runner = {
        let deliverer = rig.deliverer.clone();
        let stop = stop.clone();
        tokio::spawn(async move { deliverer.run(stop).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    to_channel(&rig, 1, json!({"text": "hello"}));
    rig.deliverer
        .deliver(RunId("run-1".into()), ProfileId("personal".into()))
        .await
        .unwrap();
    for _ in 0..200 {
        if !settled(&rig).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(settled(&rig).len(), 1);
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap();
}
