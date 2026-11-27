//! `SlackGateway::with_ux` integration tests: revocation, missing user, stale process, transient cancel failure, overlapping runs, and watch timeout.
use bot_core::{
    port::Error,
    profile::Adapter,
    state::{Conversation, ConversationKind, RunState},
};
use bot_gateway::{
    delivery::Sender,
    reply::ReplyBody,
    slack::{
        frame::{Button, Frame, StopRequested},
        ux::{STOP_CONFIRM, STOP_CONTINUE},
    },
    testing::{
        ports::{MemoryActive, outgoing},
        slack::{button_frame, mention_frame},
        *,
    },
};
use std::{sync::Arc, time::Duration};

use crate::support::{
    personal, settle,
    ux::{
        UnavailableActive, personal_with_admin, stop_requested_frame, thread_mention_frame, ux_rig,
    },
};

#[tokio::test]
async fn revoked_requester_cannot_stop_their_own_run_but_active_admin_can() {
    let rig = ux_rig(vec![personal_with_admin()], None, None).await;
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "10.1",
            "<@UBOT> start task",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E1".into())).await);
    assert!(
        rig.slack
            .until(|s| !s.calls("chat.startStream").is_empty())
            .await
    );

    // Revoke U1's membership, leaving only U2 (admin).
    let without_u1 = profile("personal")
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .member("slack:home/U2", &["admin"])
        .role("admin", &["run"])
        .policy_scopes(&["run"])
        .build();
    rig.catalog.replace([without_u1]);

    rig.slack
        .push(stop_requested_frame("E2", "C1", "10.1", Some("U1")))
        .await;
    rig.slack
        .push(thread_mention_frame(
            "E3",
            "C1",
            "U1",
            "10.2",
            "10.1",
            "<@UBOT> stop",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 3).await);
    assert!(
        rig.admin.cancels().is_empty(),
        "revoked requester must not be able to stop"
    );
    assert!(
        rig.slack.calls("chat.postMessage").is_empty(),
        "must not post confirmation card"
    );
    assert!(
        !rig.slack.calls("chat.postEphemeral").is_empty(),
        "expected permission-denied ephemeral notice"
    );

    rig.slack
        .push(thread_mention_frame(
            "E4",
            "C1",
            "U2",
            "10.3",
            "10.1",
            "<@UBOT> stop",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 4).await);
    let cancels = rig.admin.cancels();
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].2.0, "slack:home/U2");
}

#[tokio::test]
async fn empty_or_missing_user_in_stop_requested_is_ignored_fail_closed() {
    let rig = ux_rig(vec![personal()], None, None).await;
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "20.1",
            "<@UBOT> work",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E1".into())).await);
    assert!(
        rig.slack
            .until(|s| !s.calls("reactions.add").is_empty())
            .await
    );

    rig.slack
        .push(stop_requested_frame("E2", "C1", "20.1", None))
        .await;
    rig.slack
        .push(stop_requested_frame("E3", "C1", "20.1", Some("")))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 3).await);

    // Direct `on_frame` with an empty user must also fail closed.
    let install = rig.gateway.plan().await.unwrap().remove(0);
    assert!(
        rig.gateway
            .on_frame(
                &install,
                Frame::StopRequested(StopRequested {
                    team: "T1".into(),
                    channel: "C1".into(),
                    thread_ts: "20.1".into(),
                    user: String::new(),
                }),
            )
            .await
    );
    assert!(rig.slack.calls("chat.postMessage").is_empty());
    assert!(rig.admin.cancels().is_empty());
}

#[tokio::test]
async fn stale_active_set_blocks_all_stop_paths_and_unavailable_is_not_acked() {
    let active = MemoryActive::new(Some("set-1"));
    let rig = ux_rig(vec![personal()], Some(active.check("set-1")), None).await;
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "30.1",
            "<@UBOT> work",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E1".into())).await);
    assert!(
        rig.slack
            .until(|s| !s.calls("reactions.add").is_empty())
            .await
    );

    rig.slack
        .push(stop_requested_frame("E2", "C1", "30.1", Some("U1")))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E2".into())).await);
    let card_ts = rig.slack.calls("chat.postMessage")[0].reply["ts"]
        .as_str()
        .unwrap()
        .to_owned();

    // Another process activates a newer set, making this process stale.
    active.set(Some("set-2"));
    let run_id = rig.store.inner.accepted()[0].run.0.clone();
    rig.slack
        .push(button_frame(
            "E3",
            "T1",
            "C1",
            "U1",
            &card_ts,
            STOP_CONFIRM,
            &run_id,
        ))
        .await;
    rig.slack
        .push(thread_mention_frame(
            "E4",
            "C1",
            "U1",
            "30.2",
            "30.1",
            "<@UBOT> stop",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 4).await);
    assert!(
        rig.admin.cancels().is_empty(),
        "stale process must not call cancel"
    );
    assert!(
        rig.slack.calls("chat.update").is_empty(),
        "stale process must not overwrite confirmation card"
    );
    assert_eq!(
        rig.store.inner.accepted().len(),
        1,
        "stale stop mention must not fall through to normal admission"
    );

    // `ActiveCheck::Unavailable` must not ack (`false`).
    let unavail = ux_rig(vec![personal()], Some(Arc::new(UnavailableActive)), None).await;
    let install = unavail.gateway.plan().await.unwrap().remove(0);
    assert!(
        !unavail
            .gateway
            .on_frame(
                &install,
                Frame::Button(Button {
                    team: "T1".into(),
                    channel: "C1".into(),
                    user: "U1".into(),
                    message_ts: "30.9".into(),
                    action_id: STOP_CONFIRM.into(),
                    value: "run-1".into(),
                }),
            )
            .await
    );
}

#[tokio::test]
async fn failed_cancel_is_not_acked_and_does_not_post_stop_ack_until_retry_succeeds() {
    let rig = ux_rig(vec![personal()], None, None).await;
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "40.1",
            "<@UBOT> work",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E1".into())).await);
    assert!(
        rig.slack
            .until(|s| !s.calls("reactions.add").is_empty())
            .await
    );

    rig.slack
        .push(stop_requested_frame("E2", "C1", "40.1", Some("U1")))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E2".into())).await);
    let card_ts = rig.slack.calls("chat.postMessage")[0].reply["ts"]
        .as_str()
        .unwrap()
        .to_owned();
    let run_id = rig.store.inner.accepted()[0].run.0.clone();

    // First cancel fails transiently: must not ack or post STOP_ACK.
    rig.admin.fail_next(Error::Unavailable);
    rig.slack
        .push(button_frame(
            "E3",
            "T1",
            "C1",
            "U1",
            &card_ts,
            STOP_CONFIRM,
            &run_id,
        ))
        .await;
    settle().await;
    assert!(
        !rig.slack.acks().contains(&"E3".into()),
        "failed cancel must not be acked so Slack redelivers"
    );
    assert!(
        rig.slack.calls("chat.update").is_empty(),
        "must not post STOP_ACK on failed cancel"
    );

    // Concurrent clicks invoke cancel once without overwriting with STOP_GONE.
    let gate = rig.admin.hold_next();
    let install = rig.gateway.plan().await.unwrap().remove(0);
    let btn = Button {
        team: "T1".into(),
        channel: "C1".into(),
        user: "U1".into(),
        message_ts: card_ts.clone(),
        action_id: STOP_CONFIRM.into(),
        value: run_id.clone(),
    };
    let gw1 = rig.gateway.clone();
    let gw2 = rig.gateway.clone();
    let (i1, i2) = (install.clone(), install);
    let (b1, b2) = (btn.clone(), btn);
    let first = tokio::spawn(async move { gw1.on_frame(&i1, Frame::Button(b1)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let second = tokio::spawn(async move { gw2.on_frame(&i2, Frame::Button(b2)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.add_permits(1);
    assert!(first.await.unwrap());
    assert!(second.await.unwrap());
    assert_eq!(
        rig.admin.cancels().len(),
        1,
        "concurrent clicks must not invoke cancel twice"
    );
    let updates = rig.slack.calls("chat.update");
    assert_eq!(updates.len(), 1);
    assert!(
        updates[0].body["text"]
            .as_str()
            .unwrap()
            .contains("Stop requested"),
        "{:?}",
        updates[0].body
    );
    // STOP_CONTINUE after confirmation must not overwrite with STOP_GONE.
    rig.slack
        .push(button_frame(
            "E4",
            "T1",
            "C1",
            "U1",
            &card_ts,
            STOP_CONTINUE,
            &run_id,
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E4".into())).await);
    assert_eq!(rig.slack.calls("chat.update").len(), 1);
}

#[tokio::test]
async fn overlapping_runs_in_one_thread_never_close_the_other_runs_stream() {
    let rig = ux_rig(
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
            "50.1",
            "<@UBOT> first request",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.startStream").len() == 1)
            .await
    );
    let ts1 = rig.slack.calls("chat.startStream")[0].reply["ts"]
        .as_str()
        .unwrap()
        .to_owned();

    rig.slack
        .push(thread_mention_frame(
            "E2",
            "C1",
            "U1",
            "50.2",
            "50.1",
            "<@UBOT> second request",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E2".into())).await);
    assert!(
        rig.slack
            .until(|s| s.calls("reactions.add").len() == 2)
            .await
    );
    settle().await;
    assert_eq!(
        rig.slack.calls("chat.startStream").len(),
        1,
        "second concurrent run must not steal or recreate the thread's open stream"
    );

    let accepted = rig.store.inner.accepted();
    assert_eq!(accepted.len(), 2);
    let run1 = accepted[0].run.clone();
    let run2 = accepted[1].run.clone();

    // Completing run1 closes run1's stream (ts1) and hands off to run2 (ts2) without closing ts2.
    let snap = personal().build();
    let mut item = outgoing(
        "personal",
        1,
        "team",
        "slack:home/U1",
        Conversation {
            kind: ConversationKind::Channel,
            id: "C1".into(),
            thread: Some("50.1".into()),
        },
        r#"{"text":"first reply"}"#,
    );
    item.key.run = run1.clone();
    let body = ReplyBody {
        text: Some("first reply".into()),
        approval: None,
        attachments: vec![],
    };
    let _ = rig.gateway.sender().send(&snap, &item, &body).await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.startStream").len() == 2)
            .await
    );
    let ts2 = rig.slack.calls("chat.startStream")[1].reply["ts"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(ts1, ts2);

    rig.store.set_progress(&run1, RunState::Succeeded);
    assert!(
        rig.slack
            .until(|s| s
                .calls("reactions.add")
                .iter()
                .any(|c| c.body["name"] == "white_check_mark"))
            .await
    );
    assert!(
        rig.slack
            .calls("chat.stopStream")
            .iter()
            .all(|c| c.body["ts"] != ts2),
        "completing run1 closed run2's active stream"
    );
    assert!(
        rig.slack
            .calls("chat.delete")
            .iter()
            .all(|c| c.body["ts"] != ts2),
        "completing run1 deleted run2's active stream"
    );
    rig.slack
        .push(thread_mention_frame(
            "E3",
            "C1",
            "U1",
            "50.3",
            "50.1",
            "<@UBOT> stop",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E3".into())).await);
    let cancels = rig.admin.cancels();
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].1, run2);
}

#[tokio::test]
async fn watch_timeout_and_graceful_shutdown_exit_cleanly_without_reacting_x() {
    // 1) Watch timeout: do not add `:x:` while the run is still `Leased`.
    let rig = ux_rig(
        vec![personal()],
        None,
        Some((Duration::from_millis(20), Duration::from_millis(120))),
    )
    .await;
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "60.1",
            "<@UBOT> long task",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| !s.calls("chat.startStream").is_empty())
            .await
    );
    assert!(
        rig.slack
            .until(|s| !s.calls("chat.stopStream").is_empty())
            .await
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    let added: Vec<String> = rig
        .slack
        .calls("reactions.add")
        .into_iter()
        .filter_map(|c| c.body["name"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(added, ["eyes"], "watch timeout must not add :x: reaction");
    assert!(rig.slack.calls("reactions.remove").is_empty());

    // 2) Graceful shutdown: do not add `:x:` for an in-flight run.
    let rig2 = ux_rig(vec![personal()], None, None).await;
    rig2.slack
        .push(mention_frame(
            "E2",
            "T1",
            "C1",
            "U1",
            "61.1",
            "<@UBOT> task during shutdown",
        ))
        .await;
    assert!(
        rig2.slack
            .until(|s| !s.calls("chat.startStream").is_empty())
            .await
    );
    rig2.stop.cancel();
    assert!(
        rig2.slack
            .until(|s| !s.calls("chat.stopStream").is_empty())
            .await
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    let added2: Vec<String> = rig2
        .slack
        .calls("reactions.add")
        .into_iter()
        .filter_map(|c| c.body["name"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        added2,
        ["eyes"],
        "graceful shutdown must not add :x: reaction"
    );
    assert!(rig2.slack.calls("reactions.remove").is_empty());
}
