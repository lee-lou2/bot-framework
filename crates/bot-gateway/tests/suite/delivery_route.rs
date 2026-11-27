//! Explicit canonical schedule route (`team/channel`) delivery and send-time host registry checks.

use crate::delivery_support::*;
use crate::support::{default_secrets, personal};
use bot_gateway::{
    host::HostConfig,
    ingress::Ingress,
    slack::{SlackGateway, api::SlackApi},
    testing::{
        ports::{MemoryBroker, MemoryLedger, outgoing},
        slack::FakeSlack,
        *,
    },
};
use serde_json::{Value, json};
use std::sync::Arc;

fn pinned(rig: &Rig, seq: u32, id: &str, body: Value) {
    rig.outbox.push(outgoing(
        "personal",
        seq,
        "team",
        "slack:home/U1",
        channel(id),
        &body.to_string(),
    ));
}

#[tokio::test]
async fn a_pinned_route_is_sent_by_channel_id_only_and_only_for_the_registered_team_and_channel() {
    let rig = rig().await;
    pinned(&rig, 1, "T1/C1", json!({"text": "pinned route"}));
    assert_eq!(rig.deliverer.tick().await.unwrap(), 1);
    // Slack API receives only the channel ID after verifying the team prefix.
    let calls = posts(&rig);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["channel"], "C1");
    assert_eq!(calls[0].body["thread_ts"], THREAD);
    assert_eq!(settled(&rig), ["confirmed:C1:1700000000.000002"]);

    // Reject wrong team, other channel, malformed routes, and lowercase team prefixes.
    for (seq, id) in [
        (2, "T9/C1"),
        (3, "T1/C2"),
        (4, "T1/C9"),
        (5, "T1/C1/C1"),
        (6, "T1/"),
        (7, "t1/C1"),
    ] {
        pinned(&rig, seq, id, json!({"text": "must not send"}));
    }
    assert_eq!(rig.deliverer.tick().await.unwrap(), 6);
    assert_eq!(
        posts(&rig).len(),
        1,
        "only the first message should reach Slack"
    );
    assert_eq!(settled(&rig)[1..], ["rejected"; 6]);
}

fn registry(team: &str, planning: &str) -> String {
    format!(
        r#"{{"slack": {{"workspaces": {{"home": {{"team_id": "{team}", "dm": true, "bot_token": "bot-token",
            "app_token": "app-token", "channels": {{"planning": "{planning}", "ops": "C2"}}}}}}}}}}"#
    )
}

#[tokio::test]
async fn a_pinned_route_is_checked_against_the_registry_at_send_time() {
    for host in [registry("T2", "C1"), registry("T1", "C7")] {
        let rig = rig_on(&host, personal(), default_secrets(), None).await;
        pinned(&rig, 1, "T1/C1", json!({"text": "stale route"}));
        assert_eq!(rig.deliverer.tick().await.unwrap(), 1, "{host}");
        assert!(rig.slack.all_calls().is_empty(), "must not send: {host}");
        assert_eq!(settled(&rig), ["rejected"], "{host}");
    }
    let rig = rig_on(&registry("T1", "C7"), personal(), default_secrets(), None).await;
    pinned(&rig, 1, "T1/C7", json!({"text": "new route"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig)[0].body["channel"], "C7");
    let rig = self::rig().await;
    rig.outbox.push(outgoing(
        "personal",
        1,
        "gone",
        "slack:home/U1",
        channel("T1/C1"),
        &json!({"text": "removed binding"}).to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    assert!(rig.slack.all_calls().is_empty());
    assert_eq!(settled(&rig), ["rejected"]);
}

#[tokio::test]
async fn a_registry_that_no_longer_matches_the_profile_leaves_the_item_alone() {
    // Missing channel or workspace in the host registry prevents both sending and settling.
    for host in [
        r#"{"slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token", "app_token": "app-token", "channels": {"ops": "C2"}}}}}"#,
        r#"{"slack": {"workspaces": {}}}"#,
    ] {
        let rig = rig_on(host, personal(), default_secrets(), None).await;
        pinned(&rig, 1, "T1/C1", json!({"text": "missing recipient"}));
        assert_eq!(rig.deliverer.tick().await.unwrap(), 1, "{host}");
        assert!(rig.slack.all_calls().is_empty(), "{host}");
        assert!(
            settled(&rig).is_empty() && rig.outbox.begun().is_empty(),
            "{host}"
        );
    }
}

/// Builds a direct `SlackSender` bypassing `Deliverer` with the given host registry.
fn direct_sender(host: &str, slack: &FakeSlack) -> bot_gateway::slack::send::SlackSender {
    let ingress = Arc::new(Ingress::new(
        TestCatalog::new([personal().build()]),
        Arc::new(HostConfig::from_json(host).unwrap()),
        MemoryStore::new(),
        MemoryPrivate::new(),
    ));
    SlackGateway::new(
        ingress,
        MemoryBroker::new(),
        MemoryLedger::new(),
        default_secrets(),
        SlackApi::new(Some(&slack.base)),
    )
    .sender()
}

#[tokio::test]
async fn the_sender_itself_refuses_a_route_the_registry_does_not_hold_even_when_called_directly() {
    // `send` enforces the registry independently of `Deliverer::allows`.
    use bot_gateway::{
        delivery::{Delivery, Sender},
        reply::ReplyBody,
    };
    let body = ReplyBody::parse(&bot_core::command::JsonPayload(
        json!({"text": "direct"}).to_string(),
    ))
    .unwrap();
    let snapshot = personal().build();
    let item =
        |seq, id: &str| outgoing("personal", seq, "team", "slack:home/U1", channel(id), "{}");

    let slack = FakeSlack::start().await;
    let sender = direct_sender(HOST, &slack);
    for (seq, id) in [(1, "T9/C1"), (2, "T1/C2"), (3, "T1/C1/C1"), (4, "C2")] {
        let got = sender.send(&snapshot, &item(seq, id), &body).await;
        assert_eq!(got, Delivery::Rejected, "{id}");
    }
    assert!(slack.all_calls().is_empty(), "Slack must not be called");
    let got = sender.send(&snapshot, &item(5, "T1/C1"), &body).await;
    assert!(matches!(got, Delivery::Sent { .. }), "{got:?}");
    let sent = slack.calls("chat.postMessage");
    assert_eq!(
        (sent.len(), sent[0].body["channel"].as_str()),
        (1, Some("C1"))
    );

    // Without the channel or workspace in the registry, neither plain `C1` nor canonical `T1/C1` is sent.
    for host in [
        registry_without_channel(),
        r#"{"slack": {"workspaces": {}}}"#.to_owned(),
    ] {
        let slack = FakeSlack::start().await;
        let sender = direct_sender(&host, &slack);
        for (seq, id) in [(1, "C1"), (2, "T1/C1")] {
            let got = sender.send(&snapshot, &item(seq, id), &body).await;
            assert_eq!(got, Delivery::Rejected, "{id} {host}");
        }
        assert!(slack.all_calls().is_empty(), "{host}");
    }
}

fn registry_without_channel() -> String {
    r#"{"slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token", "app_token": "app-token", "channels": {"ops": "C2"}}}}}"#.to_owned()
}

#[tokio::test]
async fn a_pinned_route_with_attachments_uploads_to_the_channel_id() {
    let rig = rig().await;
    pinned(
        &rig,
        1,
        "T1/C1",
        json!({"text": "report", "attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]}),
    );
    pinned(
        &rig,
        2,
        "T9/C1",
        json!({"text": "other team", "attachments": [{"name": "b.txt", "type": "text/plain", "data": "aGk="}]}),
    );
    rig.deliverer.tick().await.unwrap();
    let done = rig.slack.calls("files.completeUploadExternal");
    assert_eq!(done.len(), 1, "wrong-team item must not start upload");
    assert_eq!(done[0].body["channel_id"], "C1");
    assert_eq!(rig.slack.calls("files.getUploadURLExternal").len(), 1);
    assert_eq!(settled(&rig)[1], "rejected");
}
