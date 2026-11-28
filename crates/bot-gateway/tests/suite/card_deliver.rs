//! Owner-DM approval card delivery (`CardDeliverer`) and confirmed receipt click verification.

use crate::delivery_support::*;
use bot_core::{
    cards::{CardJob, CardKey, CardOutcome, CardReceipt, CardSource, LABEL_RECIPIENT},
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId},
    intake::NotSentReason,
    port::Error,
    profile::{Adapter, Snapshot},
};
use bot_gateway::{
    slack::{cards::is_the_card, frame::Button, socket::Installation},
    testing::{cards::MemoryCards, profile, slack::Scripted},
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

async fn fresh() -> Rig {
    rig().await
}

fn job(nonce: &str, owner: &str, binding: &str) -> CardJob {
    CardJob {
        key: CardKey {
            nonce: ApprovalId(nonce.into()),
            attempt: 0,
        },
        profile: ProfileId("personal".into()),
        binding: BindingId(binding.into()),
        owner: PrincipalId(owner.into()),
        card: JsonPayload(
            json!({
                "nonce": nonce,
                "title": "Scheduled delivery approval",
                "lines": [format!("{LABEL_RECIPIENT}: team channel")]
            })
            .to_string(),
        ),
        generation: Generation(1),
        source: None,
    }
}

fn posted(rig: &Rig) -> usize {
    rig.slack.calls("chat.postMessage").len()
}

#[tokio::test]
async fn the_first_sender_posts_the_card_to_the_owners_dm_and_the_receipt_is_settled() {
    let rig = fresh().await;
    rig.slack.set_url(Some("https://home.slack.com/"));
    let cards = MemoryCards::new();
    let mut card_job = job("n-1", "slack:home/U1", "team");
    card_job.source = Some(CardSource {
        channel: "C1".into(),
        thread_ts: Some("1700000000.000100".into()),
        message_ts: None,
    });
    cards.push(card_job);
    let deliverer = rig.card_deliverer(cards.clone());
    assert_eq!(deliverer.tick().await.unwrap(), 1);
    assert_eq!(
        cards.begun(),
        [("n-1".to_owned(), "personal-r1".to_owned())]
    );
    assert_eq!(rig.slack.calls("conversations.open")[0].body["users"], "U1");
    assert_eq!(posted(&rig), 1);
    let sent = &rig.slack.calls("chat.postMessage")[0];
    assert_eq!(sent.body["channel"], "D1");
    assert_eq!(
        sent.body["blocks"][1]["elements"][0]["text"],
        "Requested in <#C1> · <https://home.slack.com/archives/C1/p1700000000000100|View thread>"
    );
    let (dm, ts) = sent.posted().unwrap();
    assert_eq!(
        cards.settled(),
        [(
            "n-1".to_owned(),
            CardOutcome::Posted {
                team: "T1".into(),
                dm,
                ts
            }
        )]
    );
    let receipt = cards.receipt(&ApprovalId("n-1".into())).expect("receipt");
    assert_eq!(
        (
            receipt.team.as_str(),
            receipt.dm.as_str(),
            receipt.owner.0.as_str()
        ),
        ("T1", "D1", "slack:home/U1")
    );
    assert_eq!(receipt.binding.0, "team");
    assert_eq!(
        receipt.source,
        Some(CardSource {
            channel: "C1".into(),
            thread_ts: Some("1700000000.000100".into()),
            message_ts: None,
        })
    );
    assert_eq!(deliverer.tick().await.unwrap(), 0);
    assert_eq!(posted(&rig), 1);
}

#[tokio::test]
async fn a_dm_that_cannot_be_opened_closes_the_request_without_posting() {
    let rig = fresh().await;
    rig.slack.script(
        "conversations.open",
        Scripted::Json(json!({"ok": false, "error": "user_not_found"})),
    );
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert_eq!(
        cards.settled(),
        [("n-1".to_owned(), CardOutcome::Unavailable)]
    );
    assert_eq!(posted(&rig), 0);
    assert!(cards.receipt(&ApprovalId("n-1".into())).is_none());
}

#[tokio::test]
async fn an_unclear_post_is_held_and_never_reposted_automatically() {
    let rig = fresh().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: String::new(),
        },
    );
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    assert_eq!(cards.settled(), [("n-1".to_owned(), CardOutcome::Held)]);
    assert!(cards.receipt(&ApprovalId("n-1".into())).is_none());
    // `begin` succeeds only for the first sender, preventing automatic reposts.
    deliverer.tick().await.unwrap();
    cards.push(job("n-1", "slack:home/U1", "team"));
    deliverer.tick().await.unwrap();
    assert_eq!(posted(&rig), 1, "no automatic repost");
}

#[tokio::test]
async fn nothing_posted_means_the_job_is_picked_up_again_and_posted_once() {
    let rig = fresh().await;
    rig.slack.script(
        "conversations.open",
        Scripted::Raw {
            status: 429,
            retry_after: Some(5),
            body: String::new(),
        },
    );
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    assert_eq!(
        cards.settled(),
        [(
            "n-1".to_owned(),
            CardOutcome::NotSent {
                reason: NotSentReason::RateLimited,
                retry_after: Some(Duration::from_secs(5))
            }
        )]
    );
    assert_eq!(posted(&rig), 0);
    assert_eq!(cards.waiting(), 1, "requeued for retry");
    deliverer.tick().await.unwrap();
    assert_eq!(posted(&rig), 1);
    assert!(matches!(cards.settled()[1].1, CardOutcome::Posted { .. }));
}

#[tokio::test]
async fn a_settle_the_store_could_not_take_is_recorded_again_without_posting_again() {
    let rig = fresh().await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    // Fail settle twice with transient store errors after posting.
    cards.fail_settle([Error::Unavailable, Error::Unavailable]);
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    assert_eq!(posted(&rig), 1);
    assert!(cards.settled().is_empty());
    deliverer.tick().await.unwrap();
    assert!(cards.settled().is_empty(), "still unavailable");
    // Once recovered, record the original receipt without reposting.
    deliverer.tick().await.unwrap();
    let (dm, ts) = rig.slack.calls("chat.postMessage")[0].posted().unwrap();
    assert_eq!(
        cards.settled(),
        [(
            "n-1".to_owned(),
            CardOutcome::Posted {
                team: "T1".into(),
                dm,
                ts
            }
        )]
    );
    deliverer.tick().await.unwrap();
    assert_eq!(cards.settled().len(), 1);
    assert_eq!(posted(&rig), 1);
}

#[tokio::test]
async fn a_settle_the_store_refused_for_good_is_not_retried() {
    let rig = fresh().await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    // `Stale` is permanent; do not retry.
    cards.fail_settle([Error::Stale]);
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    deliverer.tick().await.unwrap();
    assert!(cards.settled().is_empty());
    assert_eq!(posted(&rig), 1);
}

#[tokio::test]
async fn a_lost_begin_or_claim_posts_nothing() {
    let rig = fresh().await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    cards.fail_begin(true);
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    assert!(rig.slack.all_calls().is_empty() && cards.settled().is_empty());
    cards.fail_claim(true);
    assert!(deliverer.tick().await.is_err());
}

#[tokio::test]
async fn a_job_that_no_longer_matches_the_current_profile_is_closed_without_any_slack_call() {
    // Missing binding, DM binding, non-member owner, malformed card, mismatched nonce.
    let mut bad_card = job("n-4", "slack:home/U1", "team");
    bad_card.card = JsonPayload("not json".into());
    let mut other_nonce = job("n-5", "slack:home/U1", "team");
    other_nonce.card = job("other", "slack:home/U1", "team").card;
    for (case, bad) in [
        ("gone", job("n-1", "slack:home/U1", "gone")),
        ("dm", job("n-2", "slack:home/U1", "direct")),
        ("stranger", job("n-3", "slack:home/U9", "team")),
        ("card", bad_card),
        ("nonce", other_nonce),
    ] {
        let rig = fresh().await;
        let cards = MemoryCards::new();
        let nonce = bad.key.nonce.clone();
        cards.push(bad);
        rig.card_deliverer(cards.clone()).tick().await.unwrap();
        assert_eq!(
            cards.settled(),
            [(nonce.0, CardOutcome::Unavailable)],
            "{case}"
        );
        assert!(rig.slack.all_calls().is_empty(), "{case}");
    }
    // Non-active profiles are left untouched (neither begun nor settled).
    let rig = fresh().await;
    let cards = MemoryCards::new();
    let mut elsewhere = job("n-6", "slack:home/U1", "team");
    elsewhere.profile = ProfileId("nobody".into());
    cards.push(elsewhere);
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert!(cards.settled().is_empty() && cards.begun().is_empty());
    assert!(rig.slack.all_calls().is_empty());
}

fn receipt() -> CardReceipt {
    CardReceipt {
        team: "T1".into(),
        dm: "D1".into(),
        ts: "1700000000.000100".into(),
        owner: PrincipalId("slack:home/U1".into()),
        profile: ProfileId("personal".into()),
        binding: BindingId("team".into()),
        source: None,
    }
}

fn install() -> Installation {
    Installation {
        profile: ProfileId("personal".into()),
        alias: "home".into(),
        team: "T1".into(),
        app_id: None,
        bot_user: "UBOT".into(),
    }
}

fn click() -> Button {
    Button {
        team: "T1".into(),
        channel: "D1".into(),
        user: "U1".into(),
        message_ts: "1700000000.000100".into(),
        action_id: "bot_approve".into(),
        value: "n-1".into(),
    }
}

fn snapshot() -> Arc<Snapshot> {
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
        .binding(
            "direct",
            Adapter::SlackDm {
                workspace: "home".into(),
            },
            "main",
        )
        .binding(
            "elsewhere",
            Adapter::SlackChannel {
                workspace: "other".into(),
                channel: "general".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .member("slack:home/U1", &["owner"])
        .build()
}

#[test]
fn only_a_click_on_the_exact_confirmed_card_by_its_owner_counts() {
    let snapshot = snapshot();
    assert!(is_the_card(&receipt(), &install(), &snapshot, &click()));
    type Edit = fn(&mut Button);
    let clicks: [(&str, Edit); 6] = [
        ("copied channel", |b| b.channel = "C1".into()),
        ("other DM", |b| b.channel = "D2".into()),
        ("other message", |b| {
            b.message_ts = "1700000000.000200".into()
        }),
        ("missing ts", |b| b.message_ts = String::new()),
        ("other user", |b| b.user = "U2".into()),
        ("other team", |b| b.team = "T9".into()),
    ];
    for (name, edit) in clicks {
        let mut button = click();
        edit(&mut button);
        assert!(
            !is_the_card(&receipt(), &install(), &snapshot, &button),
            "{name}"
        );
    }
    type Reinstall = fn(&mut Installation);
    let installs: [(&str, Reinstall); 3] = [
        ("other profile", |i| i.profile = ProfileId("work".into())),
        ("other team install", |i| i.team = "T9".into()),
        ("other install alias", |i| i.alias = "other".into()),
    ];
    for (name, edit) in installs {
        let mut install = install();
        edit(&mut install);
        assert!(
            !is_the_card(&receipt(), &install, &snapshot, &click()),
            "{name}"
        );
    }
    let mut other_team = receipt();
    other_team.team = "T9".into();
    assert!(!is_the_card(&other_team, &install(), &snapshot, &click()));
    // Empty receipt ts never matches, even against a click without ts.
    let mut no_ts = receipt();
    no_ts.ts = String::new();
    let mut blank = click();
    blank.message_ts = String::new();
    assert!(!is_the_card(&no_ts, &install(), &snapshot, &blank));
    let mut other_owner = receipt();
    other_owner.owner = PrincipalId("slack:home/U2".into());
    assert!(!is_the_card(&other_owner, &install(), &snapshot, &click()));
    // Original binding must still be a Slack channel binding on this installation.
    for binding in ["gone", "direct", "elsewhere"] {
        let mut moved = receipt();
        moved.binding = BindingId(binding.into());
        assert!(
            !is_the_card(&moved, &install(), &snapshot, &click()),
            "{binding}"
        );
    }
}
