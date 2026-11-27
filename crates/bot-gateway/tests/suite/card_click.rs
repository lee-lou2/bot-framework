//! Owner-DM approval card click verification against confirmed delivery receipts.
use crate::support::*;
use bot_core::{
    cards::{CardJob, CardKey, LABEL_RECIPIENT},
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId},
    ledger::ApprovalState,
    port::Error,
    profile::Adapter,
};
use bot_gateway::{
    slack::cards::CardDeliverer,
    testing::{cards::MemoryCards, profile, slack::button_frame},
};
use serde_json::json;

const NONCE: &str = "n1";

/// Channel-only profile (no plain DM input binding); owner U1, other member U2.
fn channel_only() -> bot_gateway::testing::Fixture {
    profile("personal")
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .member("slack:home/U1", &["owner"])
        .member("slack:home/U2", &["owner"])
        .role("owner", &["run"])
        .policy_scopes(&["run"])
}

fn card_json() -> String {
    json!({
        "nonce": NONCE,
        "title": "Scheduled delivery approval",
        "lines": [format!("{LABEL_RECIPIENT}: Team channel")],
    })
    .to_string()
}

fn job() -> CardJob {
    CardJob {
        key: CardKey {
            nonce: ApprovalId(NONCE.into()),
            attempt: 0,
        },
        profile: ProfileId("personal".into()),
        binding: BindingId("team".into()),
        owner: PrincipalId("slack:home/U1".into()),
        card: JsonPayload(card_json()),
        generation: Generation(1),
    }
}

async fn delivered(profiles: Vec<bot_gateway::testing::Fixture>) -> (Rig, String, String) {
    let rig = connected(profiles).await;
    let cards = MemoryCards::linked(rig.ledger.clone());
    rig.ledger
        .put(NONCE, "personal", ApprovalState::Pending, &card_json());
    cards.push(job());
    let deliverer = CardDeliverer::new(cards, rig.ingress.clone(), rig.gateway.sender());
    assert_eq!(deliverer.tick().await.unwrap(), 1);
    let (dm, ts) = rig.slack.calls("chat.postMessage")[0]
        .posted()
        .expect("card was posted");
    (rig, dm, ts)
}

fn ephemerals(rig: &Rig) -> Vec<String> {
    rig.slack
        .calls("chat.postEphemeral")
        .iter()
        .map(|c| c.body["text"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_owner_decides_on_the_dm_card_even_when_plain_dm_input_is_closed() {
    let (rig, dm, ts) = delivered(vec![channel_only()]).await;
    assert_eq!(dm, "D1");
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            &dm,
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(
        rig.broker.decisions(),
        [(NONCE.to_string(), "slack:home/U1".to_string(), true)]
    );
    rig.ledger.set_state(NONCE, ApprovalState::Approved);
    rig.slack
        .push(button_frame("B2", "T1", &dm, "U1", &ts, "bot_deny", NONCE))
        .await;
    assert!(rig.slack.until(|s| s.acks() == ["B1", "B2"]).await);
    let update = &rig.slack.calls("chat.update")[0];
    assert_eq!(
        (update.body["channel"].as_str(), update.body["ts"].as_str()),
        (Some(dm.as_str()), Some(ts.as_str()))
    );
    assert!(ephemerals(&rig).is_empty());
}

#[tokio::test]
async fn a_click_anywhere_but_the_confirmed_dm_message_decides_nothing() {
    let (rig, dm, ts) = delivered(vec![channel_only()]).await;
    for (id, channel, user, message_ts) in [
        ("B1", "C1", "U1", ts.as_str()),
        ("B2", "D2", "U1", ts.as_str()),
        ("B3", dm.as_str(), "U1", "1700000000.999999"),
        ("B4", dm.as_str(), "U2", ts.as_str()),
        ("B5", dm.as_str(), "U1", ""),
    ] {
        rig.slack
            .push(button_frame(
                id,
                "T1",
                channel,
                user,
                message_ts,
                "bot_approve",
                NONCE,
            ))
            .await;
    }
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 5)
            .await
    );
    // Permanently invalid clicks must be acked to avoid redelivery loops.
    assert!(
        rig.slack
            .until(|s| ["B1", "B2", "B3", "B4", "B5"]
                .iter()
                .all(|id| s.acks().contains(&(*id).to_string())))
            .await
    );
    assert!(rig.broker.decisions().is_empty());
    assert_eq!(rig.broker.attempts(), 0);
    assert!(
        ephemerals(&rig).iter().all(|text| {
            let lower = text.to_ascii_lowercase();
            lower.contains("approv")
                && (lower.contains("cannot") || lower.contains("can't") || lower.contains("not"))
        }),
        "{:?}",
        ephemerals(&rig)
    );
    rig.slack
        .push(button_frame(
            "B6",
            "T9",
            &dm,
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"B6".to_string()))
            .await
    );
    assert_eq!(rig.broker.attempts(), 0);
}

#[tokio::test]
async fn a_click_before_the_receipt_is_confirmed_decides_nothing_and_says_to_try_again() {
    let rig = connected(vec![channel_only()]).await;
    rig.ledger
        .put(NONCE, "personal", ApprovalState::Pending, &card_json());
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "D1",
            "U1",
            "1700000000.000001",
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(rig.broker.attempts(), 0);
    let notices = ephemerals(&rig);
    assert!(
        notices.len() == 1 && {
            let lower = notices[0].to_ascii_lowercase();
            lower.contains("open") || lower.contains("again")
        },
        "{notices:?}"
    );
    assert_eq!(rig.slack.calls("chat.postEphemeral")[0].body["user"], "U1");
}

#[tokio::test]
async fn a_click_on_a_closed_dm_card_is_explained_to_its_owner_and_decides_nothing() {
    let rig = connected(vec![channel_only()]).await;
    rig.ledger
        .put(NONCE, "personal", ApprovalState::Denied, &card_json());
    for (envelope, channel, user) in [
        ("B1", "D1", "U1"),
        // Non-members and non-DM channels receive no notice.
        ("B2", "D1", "U9"),
        ("B3", "C9", "U1"),
    ] {
        rig.slack
            .push(button_frame(
                envelope,
                "T1",
                channel,
                user,
                "1700000000.000001",
                "bot_approve",
                NONCE,
            ))
            .await;
    }
    assert!(
        rig.slack
            .until(|s| ["B1", "B2", "B3"]
                .iter()
                .all(|id| s.acks().contains(&(*id).to_string())))
            .await
    );
    assert_eq!(rig.broker.attempts(), 0);
    let notices = ephemerals(&rig);
    assert!(
        notices.len() == 1 && notices[0].to_ascii_lowercase().contains("closed"),
        "{notices:?}"
    );
    assert_eq!(rig.slack.calls("chat.postEphemeral")[0].body["user"], "U1");
    // Approvals from another profile are treated as unknown even on a matching DM click.
    rig.ledger
        .put("n2", "work", ApprovalState::Denied, &card_json());
    rig.slack
        .push(button_frame(
            "B4",
            "T1",
            "D1",
            "U1",
            "1700000000.000001",
            "bot_approve",
            "n2",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"B4".to_string()))
            .await
    );
    assert_eq!(ephemerals(&rig).len(), 1);
}

#[tokio::test]
async fn an_approval_that_cannot_be_read_is_retried_not_decided_elsewhere() {
    // Even with plain DM input open, unreadable ledger state must not fall back or ack.
    let (rig, dm, ts) = delivered(vec![personal()]).await;
    rig.ledger.fail_next(Error::Unavailable);
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            &dm,
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 1)
            .await
    );
    assert_eq!(rig.broker.attempts(), 0);
    assert!(
        rig.slack.acks().is_empty(),
        "transient failure must not be acked"
    );
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            &dm,
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(rig.broker.decisions().len(), 1);
}

#[tokio::test]
async fn a_dm_card_cannot_be_decided_from_the_plain_dm_input_path_or_by_a_non_member() {
    let (rig, _dm, ts) = delivered(vec![personal()]).await;
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "D7",
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 1)
            .await
    );
    assert_eq!(rig.broker.attempts(), 0);
    // Revoking membership blocks decisions even when the receipt matches.
    let (rig, dm, ts) = delivered(vec![channel_only()]).await;
    rig.catalog.replace([channel_only_without_owner().build()]);
    settle().await;
    rig.slack
        .push(button_frame(
            "B2",
            "T1",
            &dm,
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks() == ["B2"]).await);
    assert_eq!(rig.broker.attempts(), 0);
    assert!(
        ephemerals(&rig)[0]
            .to_ascii_lowercase()
            .contains("registered"),
        "{:?}",
        ephemerals(&rig)
    );
}

fn channel_only_without_owner() -> bot_gateway::testing::Fixture {
    profile("personal")
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .member("slack:home/U2", &["owner"])
        .role("owner", &["run"])
        .policy_scopes(&["run"])
}

#[tokio::test]
async fn dm_approval_notice_keeps_stream_resumable_until_final_reply() {
    use bot_core::{
        cards::DM_SENT_NOTICE,
        state::{Conversation, ConversationKind, RunState},
    };
    use bot_gateway::{
        delivery::Sender,
        reply::ReplyBody,
        testing::{ports::outgoing, slack::mention_frame},
    };
    use std::time::Duration;

    let rig = crate::support::ux::ux_rig(
        vec![personal()],
        None,
        Some((Duration::from_millis(20), Duration::from_secs(30))),
    )
    .await;
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "500.1",
            "<@UBOT> img",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.startStream").len() == 1)
            .await
    );
    let run = rig.store.inner.accepted()[0].run.clone();
    let snap = personal().build();
    let sender = rig.gateway.sender();
    let make = |seq: u32, text: &str| {
        let mut item = outgoing(
            "personal",
            seq,
            "team",
            "slack:home/U1",
            Conversation {
                kind: ConversationKind::Channel,
                id: "C1".into(),
                thread: Some("500.1".into()),
            },
            &format!(r#"{{"text":"{text}"}}"#),
        );
        item.key.run = run.clone();
        (
            item,
            ReplyBody {
                text: Some(text.into()),
                approval: None,
                attachments: vec![],
            },
        )
    };

    // Posting the DM-sent notice closes the prior card with `processing` and reopens below it.
    let (notice_item, notice_body) = make(0, DM_SENT_NOTICE);
    let _ = sender.send(&snap, &notice_item, &notice_body).await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.startStream").len() == 2)
            .await
    );
    assert_eq!(
        rig.slack.calls("chat.stopStream")[0].body["session_status"],
        "processing"
    );

    // Resumed tool progress updates the reopened stream until the real final reply arrives.
    rig.store.set_progress_detail(
        &run,
        RunState::Leased,
        Some("main/media"),
        Some("shuut.generate-image"),
    );
    assert!(
        rig.slack
            .until(|s| s
                .calls("chat.appendStream")
                .iter()
                .any(|c| { c.body["chunks"][0]["title"] == "Running tool: shuut.generate-image" }))
            .await
    );

    let (final_item, final_body) = make(1, "Here is your image.");
    let _ = sender.send(&snap, &final_item, &final_body).await;
    rig.store.set_progress(&run, RunState::Succeeded);
    assert!(
        rig.slack
            .until(|s| s.calls("chat.stopStream").len() == 2)
            .await
    );
    assert!(
        rig.slack.calls("chat.stopStream")[1]
            .body
            .get("session_status")
            .is_none()
    );
}
