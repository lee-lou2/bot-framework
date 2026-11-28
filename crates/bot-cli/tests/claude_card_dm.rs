//! End-to-end acceptance tests for owner-DM approval cards on channel-started schedules (fake Slack, real PG, real `claude` CLI with fake upstream).
//! Run: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-cli --test claude_card_dm`.
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use bot_gateway::testing::slack::{Call, FakeSlack, Scripted, button_frame, mention_frame};
use claude_support::*;
use process_support::Schema;
use serde_json::json;
use std::time::Duration;

const MENTION: &str = "<@UBOT> Let the team channel know when the materials arrive";
const ROLES: [Role; 3] = [Role::Gateway, Role::Scheduler, Role::Worker];

fn create_turns(rig: &Rig, n: u32, after: u64) {
    rig.script(tool_turn(
        &format!("msg_c{n}"),
        &format!("toolu_c{n}"),
        "mcp__relay__schedule_create",
        &create_with(team_channel("planning"), &in_secs(after)),
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_started_schedule_asks_the_owner_in_a_dm_and_only_that_card_decides_it() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let slack = FakeSlack::start().await;
    let mut rig = Rig::start_slack(
        schema,
        claude,
        CARD_PROFILE,
        &card_gateway(),
        &ROLES,
        &slack.base,
    )
    .await;
    assert!(slack.connected().await, "Slack socket did not connect");

    // 1) Owner starts a schedule in origin channel (ops, C2) targeting registered team channel.
    create_turns(&rig, 1, 25);
    rig.script(final_turn("msg_c2", "Scheduled."));
    rig.script(final_turn("msg_f1", "The materials have arrived."));
    let first = ts(1);
    slack
        .push(mention_frame("E1", "T1", "C2", "U1", &first, MENTION))
        .await;
    rig.until("creation approval did not open", 90, |rig| {
        rig.query(PENDING) == "1"
    })
    .await;
    let nonce = pending_nonce(&rig);

    // 2) Full approval card goes only to owner's DM (D1), never to the origin channel.
    let card = posted_card(&slack, &nonce).await;
    assert_eq!(card.channel, "D1", "card must go to owner DM");
    assert_eq!(slack.calls("conversations.open")[0].body["users"], "U1");
    let dm_post = slack
        .calls("chat.postMessage")
        .into_iter()
        .find(|call| call.body["channel"] == "D1")
        .unwrap();
    assert!(dm_post.body.get("thread_ts").is_none_or(|t| t.is_null()));
    assert!(has_buttons(&dm_post));
    let expected_url = format!("https://slack.com/archives/C2/p{}", first.replace('.', ""));
    assert_eq!(
        dm_post.body["blocks"][1]["elements"][0]["text"],
        format!("Requested in <#C2> · <{expected_url}|View thread>")
    );
    let body = dm_post.body["text"].as_str().unwrap();
    assert!(body.contains("T1/C1"), "{body}");
    assert!(body.contains(&expected_url), "{body}");

    // 3) Once posted, origin thread gets a single buttonless notice line with no draft/recipient/nonce.
    rig.until("card delivery did not confirm", 60, |rig| {
        job_state(rig, &nonce) == "posted"
    })
    .await;
    rig.until("origin channel notice was not sent", 60, |_| {
        !texts_to(&slack, "C2").is_empty()
    })
    .await;
    let notice = posts_to(&slack, "C2");
    assert_eq!(notice.len(), 1, "{}", dump(&rig));
    assert_eq!(notice[0].body["text"], DM_NOTICE);
    assert_eq!(notice[0].body["thread_ts"], first.as_str());
    assert!(!has_buttons(&notice[0]));
    assert!(posts_to(&slack, "C1").is_empty());

    // 4) Clicks from wrong channel, DM, message ts, user, or team must not consume the approval.
    let mut wrong = Vec::new();
    for (envelope, channel, user, message_ts, team) in [
        ("X1", "C2", "U1", card.ts.as_str(), "T1"),
        ("X2", "D9", "U1", card.ts.as_str(), "T1"),
        ("X3", "D1", "U1", "1700000000.999999", "T1"),
        ("X4", "D1", "U2", card.ts.as_str(), "T1"),
        ("X5", "D1", "U1", card.ts.as_str(), "T9"),
    ] {
        wrong.push(envelope);
        slack
            .push(button_frame(
                envelope,
                team,
                channel,
                user,
                message_ts,
                "bot_approve",
                &card.value,
            ))
            .await;
    }
    assert!(
        slack
            .until(|s| wrong.iter().all(|id| s.acks().contains(&(*id).to_string())))
            .await,
        "permanently unprocessable clicks must all be acked"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(rig.query(PENDING), "1", "{}", dump(&rig));
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_audit where action like 'approval.decide%'"),
        "0"
    );

    // 5) Owner approving the DM card creates one schedule; redelivered clicks are idempotent.
    click(&slack, "B1", &card).await;
    rig.until("schedule was not created", 90, |rig| {
        rig.query("select count(*) from {s}.bot_user_schedules where state = 'active'") == "1"
    })
    .await;
    assert_eq!(
        rig.query(
            "select dest_binding || '|' || dest_kind || '|' || dest_id || '|' || origin_binding || '|' || origin_id \
             from {s}.bot_user_schedules"
        ),
        "team|channel|T1/C1|ops|C2",
        "{}",
        dump(&rig)
    );
    click(&slack, "B1-again", &card).await;
    assert!(
        slack
            .until(|s| s.acks().contains(&"B1-again".to_string()))
            .await
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "1"
    );

    // 6) Firing freezes a delivery draft and sends a new approval card to owner's DM; destination stays empty.
    rig.until(
        "delivery draft did not freeze awaiting approval",
        90,
        |rig| {
            rig.query("select count(*) from {s}.bot_deliveries where state = 'drafted'") == "1"
                && rig.query(PENDING) == "1"
        },
    )
    .await;
    let draft_nonce = pending_nonce(&rig);
    assert_ne!(draft_nonce, nonce);
    let draft = posted_card(&slack, &draft_nonce).await;
    assert_eq!(draft.channel, "D1");
    assert_ne!(draft.ts, card.ts);
    assert!(
        posts_to(&slack, "C1").is_empty(),
        "nothing may be sent to destination before approval"
    );
    // Stale message_ts from the prior card must not decide the new approval.
    let stale = PostedCard {
        channel: card.channel.clone(),
        ts: card.ts.clone(),
        value: draft.value.clone(),
    };
    click(&slack, "X6", &stale).await;
    assert!(slack.until(|s| s.acks().contains(&"X6".to_string())).await);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(rig.query(PENDING), "1", "{}", dump(&rig));

    // 7) Approving the draft card delivers once to destination C1.
    click(&slack, "B2", &draft).await;
    rig.until("approved draft was not delivered", 90, |rig| {
        rig.query("select state from {s}.bot_outbox where conv_id = 'T1/C1'") == "confirmed"
    })
    .await;
    assert_eq!(posts_to(&slack, "C1").len(), 1, "{}", dump(&rig));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(posts_to(&slack, "C1").len(), 1, "must not redeliver");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dm_that_cannot_be_opened_or_a_lost_post_closes_the_request_and_says_so_once_in_the_channel()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let slack = FakeSlack::start().await;
    let rig = Rig::start_slack(
        schema,
        claude,
        CARD_PROFILE,
        &card_gateway(),
        &ROLES,
        &slack.base,
    )
    .await;
    assert!(slack.connected().await, "Slack socket did not connect");
    let closed = "select count(*) from {s}.bot_audit where action = 'approval.dm_unavailable' and verdict = 'approval_dm_unavailable'";

    // A) Owner DM cannot be opened: close approval, create no schedule/card/delivery, post unavailable notice once.
    slack.script(
        "conversations.open",
        Scripted::Json(json!({"ok": false, "error": "user_not_found"})),
    );
    create_turns(&rig, 1, 3600);
    rig.script(final_turn(
        "msg_c2",
        "Could not schedule because the approval DM could not be sent.",
    ));
    let (first, second) = (ts(1), ts(2));
    slack
        .push(mention_frame("E1", "T1", "C2", "U1", &first, MENTION))
        .await;
    wait(&rig, "first request did not finish", |rig| {
        rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'") == "1"
    })
    .await;
    assert_eq!(rig.query(closed), "1", "{}", dump(&rig));
    assert_eq!(rig.query(PENDING), "0");
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    assert!(
        slack
            .calls("chat.postMessage")
            .iter()
            .all(|call| call.body["channel"] != "D1"),
        "nothing may be posted to an unopened DM"
    );
    wait(&rig, "unavailable notice was not sent", |_| {
        !said_in(&slack, "C2", DM_UNAVAILABLE_NOTICE).is_empty()
    })
    .await;
    let lines = said_in(&slack, "C2", DM_UNAVAILABLE_NOTICE);
    assert_eq!(lines.len(), 1, "{}", dump(&rig));
    assert_eq!(lines[0].body["thread_ts"], first.as_str());
    assert!(!has_buttons(&lines[0]));
    assert!(
        said_in(&slack, "C2", DM_NOTICE).is_empty(),
        "must not post sent notice"
    );
    assert!(said_in(&slack, "C2", DM_UNCONFIRMED_NOTICE).is_empty());
    assert!(posts_to(&slack, "C1").is_empty());

    // B) Post response lost (502): close without retrying and post unconfirmed notice once.
    // Wait for A's notice and final reply to flush before scripting the 502 on chat.postMessage.
    wait(&rig, "first reply did not flush", |_| {
        texts_to(&slack, "C2").len() == 2
    })
    .await;
    slack.script(
        "chat.postMessage",
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: String::new(),
        },
    );
    create_turns(&rig, 3, 3600);
    rig.script(final_turn(
        "msg_c4",
        "Could not schedule because the delivery response was not confirmed.",
    ));
    slack
        .push(mention_frame("E2", "T1", "C2", "U1", &second, MENTION))
        .await;
    wait(&rig, "second request did not finish", |rig| {
        rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'") == "2"
    })
    .await;
    assert_eq!(rig.query(closed), "2", "{}", dump(&rig));
    let attempts = slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|call| call.body["channel"] == "D1")
        .count();
    assert_eq!(attempts, 1, "must not retry posting the card");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        slack
            .calls("chat.postMessage")
            .into_iter()
            .filter(|call| call.body["channel"] == "D1")
            .count(),
        1
    );
    assert_eq!(rig.query(PENDING), "0");
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_card_jobs where state = 'closed'"),
        "2"
    );
    wait(&rig, "unconfirmed notice was not sent", |_| {
        !said_in(&slack, "C2", DM_UNCONFIRMED_NOTICE).is_empty()
    })
    .await;
    let unsure = said_in(&slack, "C2", DM_UNCONFIRMED_NOTICE);
    assert_eq!(unsure.len(), 1, "{}", dump(&rig));
    assert_eq!(unsure[0].body["thread_ts"], second.as_str());
    assert_eq!(said_in(&slack, "C2", DM_UNAVAILABLE_NOTICE).len(), 1);
    assert!(
        said_in(&slack, "C2", DM_NOTICE).is_empty(),
        "must not post sent notice"
    );
    assert!(posts_to(&slack, "C1").is_empty());

    // C) Clicking a card from B that actually landed in DM does not decide; owner sees an ephemeral closed notice.
    let stale = rig.query(
        "select nonce from {s}.bot_card_jobs where state = 'closed' order by created_ms desc limit 1",
    );
    slack
        .push(button_frame(
            "S1",
            "T1",
            "D1",
            "U1",
            "1700000000.000009",
            "bot_approve",
            &stale,
        ))
        .await;
    assert!(slack.until(|s| s.acks().contains(&"S1".to_string())).await);
    let notices = slack.calls("chat.postEphemeral");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].body["user"], "U1");
    assert!(
        notices[0].body["text"]
            .as_str()
            .is_some_and(|text| text.contains("already closed")),
        "{}",
        notices[0].body
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    rig.stop().await;
}

/// Token rotation in flight: rejected old token retries with backoff, picks up the rotated token, and posts once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dm_card_requested_while_the_bot_token_is_replaced_still_reaches_the_owner_once() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let slack = FakeSlack::start().await;
    let secrets = Rotating::new(&[
        ("model-key", "provider-key-for-personal"),
        ("bot-token", "xoxb-old"),
        ("app-token", "xapp-test-token"),
    ]);
    let mut rig = Rig::launch(
        schema,
        claude,
        CARD_PROFILE,
        &[],
        &card_gateway(),
        &ROLES,
        secrets.clone(),
        "",
        Some(&slack.base),
    )
    .await;
    rig.until("serve did not publish an active revision", 30, |rig| {
        !rig.schema.active().is_empty()
    })
    .await;
    assert!(slack.connected().await, "Slack socket did not connect");

    // Rotate secret in store and revoke the cached startup token in Slack.
    secrets.set("bot-token", "xoxb-new");
    slack.revoke("xoxb-old", "invalid_auth");
    create_turns(&rig, 1, 3600);
    rig.script(final_turn("msg_c2", "Scheduled."));
    slack
        .push(mention_frame("E1", "T1", "C2", "U1", &ts(1), MENTION))
        .await;
    rig.until("creation approval did not open", 90, |rig| {
        rig.query(PENDING) == "1"
    })
    .await;
    let nonce = pending_nonce(&rig);

    rig.until("card delivery did not confirm", 90, |rig| {
        job_state(rig, &nonce) == "posted"
    })
    .await;
    assert_eq!(
        rig.query("select count(*) from {s}.bot_audit where action = 'approval.dm_unavailable'"),
        "0",
        "{}",
        dump(&rig)
    );
    let tokens_of = |method: &str| -> Vec<String> {
        slack
            .calls(method)
            .into_iter()
            .map(|call| call.token)
            .collect()
    };
    // Relies on the 30s startup token cache still being warm when the card sends.
    let stale = "old-token rejection path was not exercised (30s startup cache may have expired)";
    assert_eq!(
        tokens_of("conversations.open"),
        ["xoxb-old", "xoxb-new"],
        "{stale}"
    );
    assert_eq!(tokens_of("auth.test"), ["xoxb-old", "xoxb-new"], "{stale}");
    let cards: Vec<Call> = slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|call| call.body["channel"] == "D1")
        .collect();
    assert_eq!(cards.len(), 1, "card must be posted to DM once");
    assert_eq!(cards[0].token, "xoxb-new");
    assert!(cards[0].posted().is_some() && has_buttons(&cards[0]));
    rig.until("origin channel notice was not sent", 60, |_| {
        !said_in(&slack, "C2", DM_NOTICE).is_empty()
    })
    .await;
    let notice = said_in(&slack, "C2", DM_NOTICE);
    assert_eq!(notice.len(), 1, "{}", dump(&rig));
    assert_eq!(notice[0].token, "xoxb-new");

    let card = posted_card(&slack, &nonce).await;
    click(&slack, "B1", &card).await;
    rig.until("schedule was not created", 90, |rig| {
        rig.query("select count(*) from {s}.bot_user_schedules where state = 'active'") == "1"
    })
    .await;
    rig.stop().await;
}
