//! Slack Socket Mode ingress acceptance tests: installation verification, admission, ack, private DM routing, and startup checks.
use crate::support::*;
use bot_core::port::Error;
use bot_gateway::{
    slack::{
        frame::{Frame, Message},
        socket::Installation,
    },
    testing::{
        slack::{dm_frame, mention_frame},
        *,
    },
};
use serde_json::json;

#[tokio::test]
async fn a_channel_mention_is_accepted_and_acked_after_the_commit() {
    let rig = connected(vec![personal()]).await;
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "1700000001.000100",
            "<@UBOT> Show today's agenda",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"E1".to_string()))
            .await
    );
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].target, "agent main Show today's agenda");
    assert_eq!(records[0].key.principal.0, "slack:home/U1");
    assert_eq!(records[0].key.request.0, "C1.1700000001.000100");
    assert_eq!(records[0].conversation, "channel:C1:1700000001.000100");
    assert_eq!(records[0].approver.as_deref(), Some("slack:home/U1"));
    assert_eq!(
        rig.slack.calls("apps.connections.open")[0].token,
        "test-app-1"
    );
    assert_eq!(rig.slack.calls("auth.test")[0].token, "test-bot-1");
}

#[tokio::test]
async fn a_dm_threads_under_the_message_and_redelivery_is_one_request() {
    let rig = connected(vec![personal()]).await;
    rig.slack
        .push(dm_frame(
            "E1",
            "T1",
            "D1",
            "U1",
            "1700000002.000200",
            "question",
        ))
        .await;
    rig.slack
        .push(dm_frame(
            "E1b",
            "T1",
            "D1",
            "U1",
            "1700000002.000200",
            "question",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 2).await);
    let records = rig.store.accepted();
    assert_eq!(
        records.len(),
        1,
        "redelivery of the same message must coalesce into one request"
    );
    assert_eq!(records[0].conversation, "direct:D1:1700000002.000200");
    assert_eq!(records[0].target, "agent main question");
    let source = records[0].source.as_ref().expect("DM message source");
    assert_eq!(
        (
            source.kind,
            source.channel.as_str(),
            source.ts.as_str(),
            source.event_id.as_str()
        ),
        (
            bot_core::slack::SourceKind::Message,
            "D1",
            "1700000002.000200",
            "Ev-E1"
        )
    );
}

#[tokio::test]
async fn foreign_unregistered_and_bot_events_are_acked_but_never_accepted() {
    let rig = connected(vec![personal()]).await;
    let bot = json!({"type": "events_api", "envelope_id": "Ebot", "payload": {"team_id": "T1", "event": {
        "type": "message", "channel_type": "im", "user": "U1", "bot_id": "B1", "text": "x", "ts": "5.5", "channel": "D1"}}});
    for frame in [
        mention_frame("Eu", "T1", "C1", "U9", "1.1", "unregistered user"),
        mention_frame("Ec", "T1", "C9", "U1", "1.2", "unbound channel"),
        mention_frame("Et", "T9", "C1", "U1", "1.3", "foreign workspace"),
        mention_frame("Ex", "T1", "C1", "U1/x", "1.4", "malformed user id"),
        bot,
    ] {
        rig.slack.push(frame).await;
    }
    assert!(rig.slack.until(|s| s.acks().len() == 5).await);
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn an_outage_or_a_rate_limit_is_not_acked_so_slack_redelivers() {
    let rig = connected(vec![personal()]).await;
    rig.store.fail_next(Error::Unavailable);
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "1.1",
            "first attempt",
        ))
        .await;
    settle().await;
    assert!(
        rig.slack.acks().is_empty(),
        "store outage must not be acked"
    );
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "1.1",
            "first attempt",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 1).await);
    assert_eq!(rig.store.accepted().len(), 1);

    let limited = rig_limited().await;
    limited
        .slack
        .push(mention_frame("L1", "T1", "C1", "U1", "2.1", "one"))
        .await;
    assert!(limited.slack.until(|s| s.acks().len() == 1).await);
    limited
        .slack
        .push(mention_frame("L2", "T1", "C1", "U1", "2.2", "two"))
        .await;
    settle().await;
    assert_eq!(
        limited.slack.acks(),
        ["L1"],
        "transient rate limit must not be acked"
    );
    assert_eq!(limited.store.accepted().len(), 1);
}

async fn rig_limited() -> Rig {
    let mut config = host("");
    config.limits.per_principal_per_min = Some(1);
    let rig = rig_with(vec![personal()], default_secrets(), config, |_| {}).await;
    assert!(rig.slack.connected().await);
    rig
}

#[tokio::test]
async fn a_private_dm_reaches_only_the_private_intake() {
    let marker = "synthetic-sensitive-marker-91c2";
    let mut secure = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    secure.private = true;
    secure.input.insert("text".into(), "body.text".into());
    let rig = connected(vec![personal().trigger("main", secure)]).await;
    rig.slack
        .push(dm_frame("E1", "T1", "D1", "U1", "3.3", marker))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 1).await);
    assert!(rig.store.accepted().is_empty());
    assert_eq!(
        rig.private.received(),
        [(
            "inbox.receive".to_string(),
            format!(r#"{{"text":"{marker}"}}"#)
        )]
    );
    // Private intake failures must not ack or fall back to normal chat intake.
    rig.private.fail_next(Error::Unavailable);
    rig.slack
        .push(dm_frame("E2", "T1", "D1", "U1", "3.4", marker))
        .await;
    settle().await;
    assert_eq!(rig.slack.acks(), ["E1"]);
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn a_hello_from_another_app_stops_the_socket_and_nothing_is_processed() {
    let rig = rig(vec![personal()], default_secrets(), |s| {
        s.set_hello_app(Some("AOTHER"))
    })
    .await;
    assert!(rig.slack.connected().await);
    rig.slack
        .push(mention_frame("E1", "T1", "C1", "U1", "1.1", "x"))
        .await;
    settle().await;
    assert!(rig.slack.acks().is_empty());
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn reconnects_after_the_server_drops_the_connection() {
    let rig = connected(vec![personal()]).await;
    rig.slack.drop_connection();
    assert!(
        rig.slack.until(|s| s.connects() == 2).await,
        "did not reconnect"
    );
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "1.1",
            "after reconnect",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"E1".to_string()))
            .await
    );
    assert_eq!(rig.store.accepted().len(), 1);
}

#[tokio::test]
async fn startup_refuses_shared_tokens_foreign_workspaces_and_missing_credentials() {
    // Sharing one app token across profiles splits events across sockets, so startup must refuse.
    let shared = secrets(&[
        ("personal", "bot-token", "test-bot-1"),
        ("personal", "app-token", "same-app-token"),
        ("work", "bot-token", "test-bot-2"),
        ("work", "app-token", "same-app-token"),
    ]);
    let mut rig = rig(vec![personal(), work()], shared, |_| {}).await;
    let error = startup_failure(&mut rig).await;
    let lower = error.0.to_ascii_lowercase();
    assert!(
        lower.contains("app token") && lower.contains("personal") && lower.contains("work"),
        "{error}"
    );
    assert!(
        !error.0.contains("same-app-token"),
        "must not leak token value"
    );

    let mut rig = self::rig(vec![personal()], default_secrets(), |s| {
        s.set_auth("T9", "UBOT")
    })
    .await;
    let error = startup_failure(&mut rig).await;
    let reason = error
        .0
        .split_once(": ")
        .map_or(error.0.as_str(), |(_, r)| r)
        .to_ascii_lowercase();
    assert!(
        reason.contains("workspace") || reason.contains("team"),
        "{error}"
    );

    let mut rig = self::rig(
        vec![personal()],
        secrets(&[("personal", "bot-token", "test-bot-1")]),
        |_| {},
    )
    .await;
    let slack_calls = rig.slack.all_calls().len();
    let error = startup_failure(&mut rig).await;
    let reason = error
        .0
        .split_once(": ")
        .map_or(error.0.as_str(), |(_, r)| r)
        .to_ascii_lowercase();
    assert!(reason.contains("token"), "{error}");
    assert_eq!(slack_calls, 0);
}

#[tokio::test]
async fn each_profile_gets_its_own_socket_and_routes_only_its_own_bindings() {
    let both = secrets(&[
        ("personal", "bot-token", "test-bot-1"),
        ("personal", "app-token", "test-app-1"),
        ("work", "bot-token", "test-bot-2"),
        ("work", "app-token", "test-app-2"),
    ]);
    let rig = rig(vec![personal(), work()], both, |_| {}).await;
    assert!(
        rig.slack.until(|s| s.connects() == 2).await,
        "expected one socket per profile"
    );
    let plan = rig.gateway.plan().await.unwrap();
    assert_eq!(plan.len(), 2);
    let install = |profile: &str| -> Installation {
        plan.iter()
            .find(|i| i.profile.0 == profile)
            .unwrap()
            .clone()
    };
    let message = Message {
        team: "T1".into(),
        channel: "C1".into(),
        direct: false,
        user: "U1".into(),
        text: "<@UBOT> hi".into(),
        ts: "9.9".into(),
        thread_ts: None,
        files: vec!["F1".into()],
        event_id: None,
    };
    assert!(
        rig.gateway
            .on_frame(&install("work"), Frame::Message(message.clone()))
            .await
    );
    assert!(rig.store.accepted().is_empty());
    assert!(
        rig.gateway
            .on_frame(&install("personal"), Frame::Message(message))
            .await
    );
    assert_eq!(rig.store.accepted().len(), 1);
    assert_eq!(rig.store.accepted()[0].key.profile.0, "personal");
}

#[tokio::test]
async fn a_message_is_admitted_under_the_table_that_proved_its_owner() {
    use bot_core::profile::Adapter;
    let rig = connected(vec![personal()]).await;
    // Reassign the channel to `work` right after the first ownership lookup.
    let work_owns_planning = profile("work")
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
        .role("owner", &["run"])
        .policy_scopes(&["run"])
        .build();
    rig.catalog.flip_after(1, [work_owns_planning]);
    rig.slack
        .push(mention_frame("E1", "T1", "C1", "U1", "1.1", "hi"))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 1).await);
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].key.profile.0, "personal",
        "must admit under the profile that owned the binding at lookup time"
    );
    rig.slack
        .push(mention_frame("E2", "T1", "C1", "U1", "1.2", "again"))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 2).await);
    assert_eq!(rig.store.accepted().len(), 1);
}

#[tokio::test]
async fn an_unvalidated_http_receive_declaration_stops_the_gateway_instead_of_being_read_as_socket()
{
    let declared = |receive: Option<&str>| {
        let mut home = json!({"team_id": "T1", "app_id": "A1", "dm": true,
            "bot_token": "bot-token", "app_token": "app-token", "signing_secret": "signing",
            "channels": {"planning": "C1"}});
        if let Some(receive) = receive {
            home["receive"] = json!(receive);
        }
        json!({"slack": {"workspaces": {"home": home}}})
    };
    // Bypass `HostConfig::from_value` to verify `SlackGateway::plan` enforces validation before calling Slack.
    let config: bot_gateway::host::HostConfig =
        serde_json::from_value(declared(Some("http"))).unwrap();
    assert!(
        config.validate().is_err(),
        "config validation must reject http receive"
    );
    let mut rig = rig_with(vec![personal()], default_secrets(), config, |_| {}).await;
    let error = startup_failure(&mut rig).await;
    assert!(
        error
            .0
            .starts_with("gateway.slack.workspaces.home.receive: "),
        "{error}"
    );
    assert!(
        rig.slack.all_calls().is_empty(),
        "must not call Slack before validation fails"
    );
    let config = bot_gateway::host::HostConfig::from_value(declared(None)).unwrap();
    let rig = rig_with(vec![personal()], default_secrets(), config, |_| {}).await;
    assert!(rig.slack.connected().await);
}
