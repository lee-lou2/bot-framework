//! Slack selective message event tests: binding `events` filtering, thread participation evidence, deduplication, and retries.
use crate::support::{events::*, *};
use bot_core::slack::{SlackEvent, SourceKind};
use bot_gateway::testing::slack::{mention_frame, post_frame, reaction_frame};
use serde_json::{Value, json};

#[tokio::test]
async fn without_events_only_mentions_and_dms_work_as_before() {
    let rig = connected(vec![channel_profile(&[SlackEvent::Mention])]).await;
    rig.threads.seed(&thread("100.1"));
    push_all(
        &rig,
        vec![
            post_frame("E1", TC1, "U1", "100.2", Some("100.1"), "thread reply"),
            reaction_frame("E2", TC1, "U1", "100.1", "white_check_mark", "U9"),
            post_frame("E3", TC1, "U1", "101.1", None, "plain channel post"),
            mention_frame("E4", "T1", "C1", "U1", "102.1", "<@UBOT> hello"),
        ],
    )
    .await;
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].target, "agent main hello");
    assert!(
        records[0]
            .source
            .as_ref()
            .is_some_and(|s| s.kind == SourceKind::Message)
    );
}

#[tokio::test]
async fn a_human_reply_is_accepted_only_in_a_thread_the_bot_took_part_in() {
    let rig = connected(vec![channel_profile(&[
        SlackEvent::Mention,
        SlackEvent::ThreadReply,
    ])])
    .await;
    rig.threads.seed(&thread("100.1"));
    push_all(
        &rig,
        vec![
            post_frame("E1", TC1, "U1", "100.2", Some("100.1"), "tell me more"),
            post_frame("E2", TC1, "U1", "200.2", Some("200.1"), "other thread"),
            post_frame("E3", TC1, "U1", "300.1", None, "top-level post"),
            post_frame("E4", TC1, "U9", "100.3", Some("100.1"), "unregistered user"),
            post_frame(
                "E5",
                ("T1", "C9"),
                "U1",
                "100.4",
                Some("100.1"),
                "other channel",
            ),
            post_frame(
                "E6",
                ("T9", "C1"),
                "U1",
                "100.5",
                Some("100.1"),
                "other team",
            ),
            event(
                "E7",
                json!({"type": "message", "subtype": "message_changed", "channel_type": "channel",
                "user": "U1", "ts": "100.6", "thread_ts": "100.1", "channel": "C1"}),
            ),
            event(
                "E8",
                json!({"type": "message", "channel_type": "channel", "user": "U7", "bot_id": "B9",
                "text": "other app", "ts": "100.7", "thread_ts": "100.1", "channel": "C1"}),
            ),
            post_frame("E9", TC1, "U1", "not-a-ts", Some("100.1"), "invalid ts"),
        ],
    )
    .await;
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1, "{records:?}");
    let r = &records[0];
    assert_eq!(r.target, "agent main tell me more");
    assert_eq!(r.key.request.0, "C1.100.2");
    assert_eq!(r.conversation, "channel:C1:100.1");
    let body: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(
        body,
        json!({"text": "tell me more", "channel": "C1", "ts": "100.2", "thread_ts": "100.1"})
    );
    let source = r.source.clone().unwrap();
    assert_eq!(
        (
            source.kind,
            source.team.as_str(),
            source.channel.as_str(),
            source.ts.as_str()
        ),
        (SourceKind::ThreadReply, "T1", "C1", "100.2")
    );
    assert_eq!(
        (source.thread_ts.as_deref(), source.event_id.as_str()),
        (Some("100.1"), "Ev-E1")
    );
    push_all(
        &rig,
        vec![post_frame(
            "E1b",
            TC1,
            "U1",
            "100.2",
            Some("100.1"),
            "tell me more",
        )],
    )
    .await;
    assert_eq!(rig.store.accepted().len(), 1);
}

#[tokio::test]
async fn bot_roots_and_human_roots_are_both_proved_by_the_bots_own_posts() {
    let rig = connected(vec![channel_profile(&[
        SlackEvent::Mention,
        SlackEvent::ThreadReply,
    ])])
    .await;
    let own = |envelope: &str, ts: &str, root: Option<&str>| {
        let mut e = json!({"type": "message", "channel_type": "channel", "user": "UBOT", "bot_id": "B1",
            "text": "bot post", "ts": ts, "channel": "C1"});
        if let Some(root) = root {
            e["thread_ts"] = json!(root);
        }
        event(envelope, e)
    };
    push_all(
        &rig,
        vec![
            own("E1", "500.1", None),
            own("E2", "600.2", Some("600.1")),
            event("E3", json!({"type": "message", "channel_type": "channel", "user": "UOTHER", "bot_id": "B9",
                "text": "other app", "ts": "700.1", "channel": "C1"})),
        ],
    )
    .await;
    assert!(rig.store.accepted().is_empty());
    let roots: Vec<String> = rig.threads.notes().into_iter().map(|n| n.root_ts).collect();
    assert_eq!(roots, ["500.1", "600.1"]);
    push_all(
        &rig,
        vec![
            post_frame("E4", TC1, "U1", "500.2", Some("500.1"), "reply to bot root"),
            post_frame(
                "E5",
                TC1,
                "U3",
                "600.3",
                Some("600.1"),
                "reply to human root",
            ),
            post_frame(
                "E6",
                TC1,
                "U1",
                "700.2",
                Some("700.1"),
                "under other app post",
            ),
        ],
    )
    .await;
    let targets: Vec<String> = rig.store.accepted().into_iter().map(|r| r.target).collect();
    assert_eq!(targets.len(), 2, "{targets:?}");
    assert!(targets.contains(&"agent main reply to bot root".to_owned()));
    assert!(targets.contains(&"agent main reply to human root".to_owned()));

    let plain = connected(vec![channel_profile(&[SlackEvent::Mention])]).await;
    push_all(&plain, vec![own("E1", "500.1", None)]).await;
    assert!(plain.threads.notes().is_empty());
}

#[tokio::test]
async fn one_post_delivered_as_mention_and_message_is_one_request_in_either_order() {
    for (events, want_reply) in [(&ALL[..2], false), (&[SlackEvent::ThreadReply][..], true)] {
        let rig = connected(vec![channel_profile(events)]).await;
        rig.threads.seed(&thread("100.1"));
        let text = "<@UBOT> please check";
        // Deliver both event orders (app_mention first, then message first) for distinct posts.
        push_all(
            &rig,
            vec![
                mention_in_thread("A1", "U1", "100.2", "100.1", text),
                post_frame("A2", TC1, "U1", "100.2", Some("100.1"), text),
                post_frame("B1", TC1, "U1", "100.3", Some("100.1"), text),
                mention_in_thread("B2", "U1", "100.3", "100.1", text),
            ],
        )
        .await;
        let records = rig.store.accepted();
        assert_eq!(records.len(), 2, "{events:?} {records:?}");
        for record in &records {
            let source = record.source.clone().unwrap();
            // Mentions take precedence over thread replies regardless of arrival order.
            assert_eq!(
                source.kind,
                if want_reply {
                    SourceKind::ThreadReply
                } else {
                    SourceKind::Message
                },
                "{events:?}"
            );
            assert_eq!(record.target, "agent main please check");
        }
        if !want_reply {
            let body: Value = serde_json::from_str(&records[0].body).unwrap();
            assert_eq!(body, json!({"text": "please check"}));
        }
    }
}

#[tokio::test]
async fn when_the_evidence_cannot_be_read_the_post_is_not_acked_and_arrives_again() {
    let rig = connected(vec![channel_profile(&[
        SlackEvent::Mention,
        SlackEvent::ThreadReply,
    ])])
    .await;
    rig.threads.seed(&thread("100.1"));
    rig.threads.fail_reads(true);
    rig.slack
        .push(post_frame(
            "E1",
            TC1,
            "U1",
            "100.2",
            Some("100.1"),
            "during outage",
        ))
        .await;
    rig.slack
        .push(mention_frame(
            "E2",
            "T1",
            "C1",
            "U1",
            "101.1",
            "<@UBOT> mention",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"E2".to_owned()))
            .await
    );
    assert!(
        !rig.slack.acks().contains(&"E1".to_owned()),
        "reply whose evidence could not be read must not be acked"
    );
    assert_eq!(
        rig.store.accepted().len(),
        1,
        "mentions do not require thread evidence"
    );
    rig.threads.fail_reads(false);
    rig.slack
        .push(post_frame(
            "E1",
            TC1,
            "U1",
            "100.2",
            Some("100.1"),
            "during outage",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"E1".to_owned()))
            .await
    );
    assert_eq!(rig.store.accepted().len(), 2);
}
