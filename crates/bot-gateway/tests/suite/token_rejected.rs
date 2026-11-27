//! Slack rejected-token handling: auth rejections settle as `NotSent` and evict cached tokens so retries verify fresh credentials.
use crate::delivery_support::*;
use crate::support;
use crate::support::default_secrets;
use bot_core::{
    cards::{CardJob, CardKey, CardOutcome, LABEL_RECIPIENT},
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId},
    intake::NotSentReason,
    ledger::ApprovalState,
};
use bot_gateway::{
    host::HostConfig,
    reply::Card,
    slack::{
        api::{Body, Raw, SlackApi},
        creds::Credentials,
        send::CardPost,
    },
    testing::{
        MemorySecrets,
        cards::MemoryCards,
        ports::outgoing,
        slack::{FakeSlack, Scripted, button_frame},
    },
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const CODES: [&str; 3] = ["invalid_auth", "token_revoked", "token_expired"];
/// Long TTL keeps the old token cached to exercise mid-rotation eviction deterministically.
const LONG: Duration = Duration::from_secs(3600);
const NOT_SENT: &str = "notsent:BeforeWrite:0";

async fn fresh() -> Rig {
    rig().await
}

async fn primed() -> (Rig, Arc<MemorySecrets>) {
    let secrets = default_secrets();
    let rig = rig_ttl(secrets.clone(), LONG).await;
    to_channel(&rig, 1, json!({"text": "first"}));
    rig.deliverer.tick().await.unwrap();
    assert!(settled(&rig)[0].starts_with("confirmed:"));
    (rig, secrets)
}

fn offer_again(rig: &Rig, seq: u32, generation: u64, body: Value) {
    let mut item = outgoing(
        "personal",
        seq,
        "team",
        "slack:home/U1",
        channel("C1"),
        &body.to_string(),
    );
    item.key.generation = Generation(generation);
    rig.outbox.push(item);
}

fn tokens(rig: &Rig) -> Vec<String> {
    posts(rig).into_iter().map(|call| call.token).collect()
}

fn auth_checks(rig: &Rig) -> Vec<String> {
    rig.slack
        .calls("auth.test")
        .into_iter()
        .map(|call| call.token)
        .collect()
}

#[tokio::test]
async fn a_reply_sent_while_the_bot_token_is_being_replaced_is_sent_once_with_the_new_token() {
    let (rig, secrets) = primed().await;
    secrets.set("personal", "bot-token", b"test-bot-2");
    rig.slack.revoke("test-bot-1", "invalid_auth");
    to_channel(&rig, 2, json!({"text": "second"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[1], NOT_SENT);
    offer_again(&rig, 2, 2, json!({"text": "second"}));
    rig.deliverer.tick().await.unwrap();
    let all = settled(&rig);
    assert_eq!(all.len(), 3, "{all:?}");
    assert!(all[2].starts_with("confirmed:C1:"), "{all:?}");
    assert!(!all.iter().any(|outcome| outcome == "rejected"), "{all:?}");
    assert_eq!(tokens(&rig), ["test-bot-1", "test-bot-1", "test-bot-2"]);
    let said: Vec<String> = posts(&rig)
        .iter()
        .filter(|call| call.succeeded())
        .map(|call| call.body["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(said, ["first", "second"], "no loss or duplication");
    assert_eq!(auth_checks(&rig), ["test-bot-1", "test-bot-2"]);
}

#[tokio::test]
async fn a_token_that_stays_rejected_is_never_final_and_is_not_posted_with_again() {
    for code in CODES {
        let (rig, _secrets) = primed().await;
        rig.slack.revoke("test-bot-1", code);
        to_channel(&rig, 2, json!({"text": "second"}));
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig)[1], NOT_SENT, "{code}");
        // Retries fail at `auth.test` without calling `chat.postMessage` again.
        for generation in 2..=3 {
            offer_again(&rig, 2, generation, json!({"text": "second"}));
            rig.deliverer.tick().await.unwrap();
        }
        let all = settled(&rig);
        assert_eq!(all[1..], [NOT_SENT, NOT_SENT, NOT_SENT], "{code}: {all:?}");
        assert_eq!(
            posts(&rig).len(),
            2,
            "{code}: only the first success and initial rejected attempt"
        );
        assert_eq!(
            auth_checks(&rig).len(),
            3,
            "{code}: initial check plus one per retry"
        );
    }
}

fn one_attachment() -> Value {
    json!({"text": "report", "attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]})
}

#[tokio::test]
async fn an_upload_treats_a_rejected_token_as_not_sent_at_every_step() {
    let (rig, _secrets) = primed().await;
    rig.slack.revoke("test-bot-1", "token_revoked");
    rig.outbox.push(outgoing(
        "personal",
        2,
        "team",
        "slack:home/U1",
        channel("C1"),
        &one_attachment().to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[1], NOT_SENT);
    assert_eq!(rig.slack.calls("files.getUploadURLExternal").len(), 1);
    assert!(rig.slack.calls("files.completeUploadExternal").is_empty());
    assert!(rig.slack.uploads().is_empty());
    offer_again(&rig, 2, 2, one_attachment());
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[2], NOT_SENT);
    assert_eq!(rig.slack.calls("files.getUploadURLExternal").len(), 1);
    assert_eq!(auth_checks(&rig).len(), 2);

    let (rig, _secrets) = primed().await;
    rig.slack.script(
        "files.completeUploadExternal",
        Scripted::Json(json!({"ok": false, "error": "token_expired"})),
    );
    rig.outbox.push(outgoing(
        "personal",
        2,
        "team",
        "slack:home/U1",
        channel("C1"),
        &one_attachment().to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[1], NOT_SENT);
    assert_eq!(rig.slack.calls("files.getUploadURLExternal").len(), 1);
    assert_eq!(rig.slack.calls("files.completeUploadExternal").len(), 1);
    assert_eq!(auth_checks(&rig).len(), 1);
    offer_again(&rig, 2, 2, one_attachment());
    rig.deliverer.tick().await.unwrap();
    assert_eq!(auth_checks(&rig).len(), 2);
    assert!(
        settled(&rig)[2].starts_with("confirmed:"),
        "{:?}",
        settled(&rig)
    );
}

#[tokio::test]
async fn the_plain_text_path_forgets_a_rejected_token_too() {
    let (rig, _secrets) = primed().await;
    rig.slack.revoke("test-bot-1", "invalid_auth");
    let long = "€".repeat(12_000);
    to_channel(&rig, 2, json!({"text": long}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[1], NOT_SENT);
    let sent = posts(&rig);
    assert_eq!(sent.len(), 2);
    assert!(sent[1].body.get("blocks").is_none(), "expected plain text");
    offer_again(&rig, 2, 2, json!({"text": long}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[2], NOT_SENT);
    assert_eq!(auth_checks(&rig).len(), 2, "must re-verify token");
    assert_eq!(
        posts(&rig).len(),
        2,
        "must not post again with rejected token"
    );
}

#[tokio::test]
async fn only_the_token_slack_refused_is_forgotten() {
    let slack = FakeSlack::start().await;
    let host = Arc::new(HostConfig::from_json(HOST).unwrap());
    let creds =
        Credentials::new(default_secrets(), host, SlackApi::new(Some(&slack.base))).with_ttl(LONG);
    let profile = ProfileId("personal".into());
    let body = json!({"channel": "C1", "text": "x"});
    let current = || async {
        creds
            .bot_token(&profile, "home")
            .await
            .map(|t| t.to_string())
    };
    assert_eq!(current().await.as_deref(), Some("test-bot-1"));
    assert_eq!(slack.calls("auth.test").len(), 1);

    creds.reject("some-older-token");
    assert_eq!(current().await.as_deref(), Some("test-bot-1"));
    assert_eq!(slack.calls("auth.test").len(), 1);

    for script in [
        Scripted::Json(json!({"ok": false, "error": "channel_not_found"})),
        Scripted::Raw {
            status: 429,
            retry_after: Some(1),
            body: String::new(),
        },
        Scripted::Raw {
            status: 500,
            retry_after: None,
            body: String::new(),
        },
    ] {
        slack.script("chat.postMessage", script);
        creds
            .call("chat.postMessage", "test-bot-1", Body::Json(&body))
            .await;
    }
    assert_eq!(current().await.as_deref(), Some("test-bot-1"));
    assert_eq!(slack.calls("auth.test").len(), 1);

    slack.revoke("test-bot-1", "invalid_auth");
    let raw = creds
        .call("chat.postMessage", "test-bot-1", Body::Json(&body))
        .await;
    assert_eq!(raw, Raw::TokenRejected);
    assert_eq!(current().await, None);
    assert_eq!(slack.calls("auth.test").len(), 2);
}

fn card() -> Card {
    Card {
        nonce: "n-1".into(),
        title: "Scheduled delivery approval".into(),
        lines: vec![format!("{LABEL_RECIPIENT}: Team channel")],
    }
}

fn not_sent() -> CardPost {
    CardPost::NotSent {
        reason: NotSentReason::BeforeWrite,
        retry_after: None,
    }
}

#[tokio::test]
async fn a_rejected_token_never_makes_the_dm_card_unavailable() {
    let owner = PrincipalId("slack:home/U1".into());
    let profile = ProfileId("personal".into());
    let rig = fresh().await;
    rig.slack.script(
        "conversations.open",
        Scripted::Json(json!({"ok": false, "error": "token_revoked"})),
    );
    let sender = rig.sender();
    let got = sender
        .post_owner_card(&profile, "home", &owner, &card())
        .await;
    assert_eq!(got, not_sent());
    assert!(posts(&rig).is_empty());

    let rig = fresh().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_auth"})),
    );
    let sender = rig.sender();
    let got = sender
        .post_owner_card(&profile, "home", &owner, &card())
        .await;
    assert_eq!(got, not_sent());
    assert_eq!(rig.slack.calls("conversations.open").len(), 1);
    assert_eq!(posts(&rig).len(), 1);
    assert_eq!(auth_checks(&rig).len(), 1);
    let got = sender
        .post_owner_card(&profile, "home", &owner, &card())
        .await;
    assert!(matches!(got, CardPost::Posted { .. }), "{got:?}");
    assert_eq!(auth_checks(&rig).len(), 2);
}

fn job() -> CardJob {
    CardJob {
        key: CardKey {
            nonce: ApprovalId("n-1".into()),
            attempt: 0,
        },
        profile: ProfileId("personal".into()),
        binding: BindingId("team".into()),
        owner: PrincipalId("slack:home/U1".into()),
        card: JsonPayload(
            json!({
                "nonce": "n-1",
                "title": "Scheduled delivery approval",
                "lines": [format!("{LABEL_RECIPIENT}: Team channel")],
            })
            .to_string(),
        ),
        generation: Generation(1),
    }
}

#[tokio::test]
async fn a_dm_card_requested_while_the_bot_token_is_being_replaced_is_posted_once_after_it() {
    let (rig, secrets) = primed().await;
    secrets.set("personal", "bot-token", b"test-bot-2");
    rig.slack.revoke("test-bot-1", "token_expired");
    let cards = MemoryCards::new();
    cards.push(job());
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    assert_eq!(
        cards.settled(),
        [(
            "n-1".to_owned(),
            CardOutcome::NotSent {
                reason: NotSentReason::BeforeWrite,
                retry_after: None
            }
        )]
    );
    assert_eq!(cards.waiting(), 1);
    assert_eq!(
        posts(&rig).len(),
        1,
        "only the initial reply should have been posted"
    );
    deliverer.tick().await.unwrap();
    let settled = cards.settled();
    assert_eq!(settled.len(), 2, "{settled:?}");
    assert!(
        matches!(settled[1].1, CardOutcome::Posted { .. }),
        "{settled:?}"
    );
    let cards_in_dm: Vec<_> = posts(&rig)
        .into_iter()
        .filter(|call| call.body["channel"] == "D1")
        .collect();
    assert_eq!(cards_in_dm.len(), 1);
    assert_eq!(cards_in_dm[0].token, "test-bot-2");
    assert!(cards_in_dm[0].posted().is_some());
}

#[tokio::test]
async fn a_card_refresh_the_token_was_refused_for_makes_the_next_use_check_the_token_again() {
    let rig = support::rig_full(
        vec![support::personal()],
        default_secrets(),
        support::host(""),
        None,
        Some(LONG),
        |slack| {
            slack.script(
                "chat.update",
                Scripted::Json(json!({"ok": false, "error": "invalid_auth"})),
            );
        },
    )
    .await;
    assert!(rig.slack.connected().await);
    let checked = rig.slack.calls("auth.test").len();
    let card =
        json!({"nonce": "n1", "title": "Delete event", "lines": ["Target: Walk"]}).to_string();
    rig.ledger
        .put("n1", "personal", ApprovalState::Approved, &card);
    for envelope in ["B1", "B2"] {
        rig.slack
            .push(button_frame(
                envelope,
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
                .until(|slack| slack.acks().contains(&envelope.to_owned()))
                .await,
            "{envelope}"
        );
    }
    let updates = rig.slack.calls("chat.update");
    assert_eq!(updates.len(), 2);
    assert!(!updates[0].succeeded() && updates[1].succeeded());
    assert_eq!(
        rig.slack.calls("auth.test").len(),
        checked + 1,
        "must re-verify token before second refresh"
    );
}

#[tokio::test]
async fn a_private_notice_the_token_was_refused_for_also_makes_the_next_use_check_the_token_again()
{
    let rig = support::rig_full(
        vec![support::personal()],
        default_secrets(),
        support::host(""),
        None,
        Some(LONG),
        |slack| {
            slack.script(
                "chat.postEphemeral",
                Scripted::Json(json!({"ok": false, "error": "token_revoked"})),
            );
        },
    )
    .await;
    assert!(rig.slack.connected().await);
    let checked = rig.slack.calls("auth.test").len();
    for envelope in ["B1", "B2"] {
        rig.slack
            .push(button_frame(
                envelope,
                "T1",
                "C1",
                "U9",
                "5.5",
                "bot_approve",
                "n1",
            ))
            .await;
        assert!(
            rig.slack
                .until(|slack| slack.acks().contains(&envelope.to_owned()))
                .await,
            "{envelope}"
        );
    }
    let notices = rig.slack.calls("chat.postEphemeral");
    assert_eq!(notices.len(), 2);
    assert!(!notices[0].succeeded() && notices[1].succeeded());
    assert_eq!(
        rig.slack.calls("auth.test").len(),
        checked + 1,
        "must re-verify token before second notice"
    );
}
