//! Sequential single-stream progress handoff tests for overlapping Slack runs in one thread.
use bot_core::{
    id::RunId,
    state::{Conversation, ConversationKind, RunState},
};
use bot_gateway::{
    delivery::Sender,
    reply::ReplyBody,
    testing::{
        ports::outgoing,
        slack::{Scripted, mention_frame},
    },
};
use serde_json::json;
use std::time::Duration;

use crate::support::{
    personal, settle,
    ux::{thread_mention_frame, ux_rig},
};

fn thread_reply(
    run: &RunId,
    seq: u32,
    thread: &str,
    text: &str,
) -> (bot_core::intake::Outgoing, ReplyBody) {
    let mut item = outgoing(
        "personal",
        seq,
        "team",
        "slack:home/U1",
        Conversation {
            kind: ConversationKind::Channel,
            id: "C1".into(),
            thread: Some(thread.into()),
        },
        &format!(r#"{{"text":"{text}"}}"#),
    );
    item.key.run = run.clone();
    let body = ReplyBody {
        text: Some(text.into()),
        approval: None,
        attachments: vec![],
    };
    (item, body)
}

#[tokio::test]
async fn concurrent_thread_runs_hand_off_single_stream_sequentially_through_all_replies() {
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
            "100.1",
            "<@UBOT> first",
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
            "100.2",
            "100.1",
            "<@UBOT> second",
        ))
        .await;
    rig.slack
        .push(thread_mention_frame(
            "E3",
            "C1",
            "U1",
            "100.3",
            "100.1",
            "<@UBOT> third",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("reactions.add").len() == 3)
            .await
    );

    let accepted = rig.store.inner.accepted();
    assert_eq!(accepted.len(), 3);
    let (r1, r2, r3) = (
        accepted[0].run.clone(),
        accepted[1].run.clone(),
        accepted[2].run.clone(),
    );

    // Update all three runs concurrently; only r1 updates the single open stream (ts1) in place.
    rig.store.set_progress_detail(
        &r1,
        RunState::Leased,
        Some("main/workspace"),
        Some("notion.search"),
    );
    rig.store.set_progress_detail(
        &r2,
        RunState::Leased,
        Some("main/workspace"),
        Some("google-workspace.read"),
    );
    rig.store
        .set_progress_detail(&r3, RunState::Leased, Some("main/workspace"), None);

    assert!(
        rig.slack
            .until(|s| s.calls("chat.appendStream").iter().any(|c| {
                c.body["ts"] == ts1 && c.body["chunks"][0]["title"] == "Running tool: notion.search"
            }))
            .await
    );
    settle().await;
    assert_eq!(rig.slack.calls("chat.startStream").len(), 1);
    assert!(rig.slack.calls("chat.stopStream").is_empty());
    assert!(rig.slack.calls("chat.delete").is_empty());
    assert!(
        rig.slack
            .calls("chat.appendStream")
            .iter()
            .all(|c| c.body["chunks"][0]["title"] == "Running tool: notion.search"),
        "non-owner runs must not overwrite r1's stream"
    );

    let snap = personal().build();

    // 1) Complete r1: closes ts1 with complete chunk + session_status=processing, posts reply, then starts r2's ts2.
    let (item1, body1) = thread_reply(&r1, 1, "100.1", "reply 1");
    let _ = rig.gateway.sender().send(&snap, &item1, &body1).await;
    rig.store.set_progress(&r1, RunState::Succeeded);
    assert!(
        rig.slack
            .until(|s| s.calls("chat.startStream").len() == 2)
            .await
    );

    let all = rig.slack.all_calls();
    let stop1 = all
        .iter()
        .position(|c| c.method == "chat.stopStream" && c.body["ts"] == ts1)
        .expect("ts1 stopped");
    let del1 = all
        .iter()
        .position(|c| c.method == "chat.delete" && c.body["ts"] == ts1)
        .expect("ts1 deleted");
    let post1 = all
        .iter()
        .position(|c| c.method == "chat.postMessage" && c.body["text"] == "reply 1")
        .expect("reply 1 posted");
    let start2 = all
        .iter()
        .rposition(|c| c.method == "chat.startStream")
        .expect("ts2 started");
    assert!(stop1 < del1 && del1 < post1 && post1 < start2);
    assert_eq!(all[stop1].body["chunks"][0]["status"], "complete");
    assert!(all[stop1].body.get("markdown_text").is_none());
    assert_eq!(all[stop1].body["session_status"], "processing");
    assert_eq!(
        all[start2].body["chunks"][0]["title"],
        "Running tool: google-workspace.read"
    );
    let ts2 = all[start2].reply["ts"].as_str().unwrap().to_owned();

    // 2) Complete r2: closes ts2 keeping session_status=processing, posts reply 2, then starts r3's ts3.
    let (item2, body2) = thread_reply(&r2, 2, "100.1", "reply 2");
    let _ = rig.gateway.sender().send(&snap, &item2, &body2).await;
    rig.store.set_progress(&r2, RunState::Succeeded);
    assert!(
        rig.slack
            .until(|s| s.calls("chat.startStream").len() == 3)
            .await
    );

    let stop2 = rig
        .slack
        .calls("chat.stopStream")
        .into_iter()
        .find(|c| c.body["ts"] == ts2)
        .expect("ts2 stopped");
    assert_eq!(stop2.body["chunks"][0]["status"], "complete");
    assert_eq!(stop2.body["session_status"], "processing");
    let start3 = rig.slack.calls("chat.startStream").pop().unwrap();
    assert_eq!(start3.body["chunks"][0]["title"], "Working as workspace");
    let ts3 = start3.reply["ts"].as_str().unwrap().to_owned();

    // 3) Complete r3 (last run): closes ts3 with default active session_status, deletes ts3, no new stream.
    let (item3, body3) = thread_reply(&r3, 3, "100.1", "reply 3");
    let _ = rig.gateway.sender().send(&snap, &item3, &body3).await;
    rig.store.set_progress(&r3, RunState::Succeeded);
    assert!(
        rig.slack
            .until(|s| s
                .calls("reactions.add")
                .iter()
                .filter(|c| c.body["name"] == "white_check_mark")
                .count()
                == 3)
            .await
    );
    settle().await;
    let stop3 = rig
        .slack
        .calls("chat.stopStream")
        .into_iter()
        .find(|c| c.body["ts"] == ts3)
        .expect("ts3 stopped");
    assert_eq!(stop3.body["chunks"][0]["status"], "complete");
    assert!(
        stop3.body.get("session_status").is_none(),
        "final run must omit session_status=processing so the session returns to active"
    );
    assert_eq!(rig.slack.calls("chat.startStream").len(), 3);
}

#[tokio::test]
async fn out_of_order_completion_skips_finished_run_and_hands_off_to_remaining_run() {
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
            "200.1",
            "<@UBOT> notion",
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
            "200.2",
            "200.1",
            "<@UBOT> gmail",
        ))
        .await;
    rig.slack
        .push(thread_mention_frame(
            "E3",
            "C1",
            "U1",
            "200.3",
            "200.1",
            "<@UBOT> calendar",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("reactions.add").len() == 3)
            .await
    );

    let accepted = rig.store.inner.accepted();
    let (r1, r2, r3) = (
        accepted[0].run.clone(),
        accepted[1].run.clone(),
        accepted[2].run.clone(),
    );
    rig.store.set_progress_detail(
        &r1,
        RunState::Leased,
        Some("main/workspace"),
        Some("notion.get"),
    );
    rig.store.set_progress_detail(
        &r3,
        RunState::Leased,
        Some("main/workspace"),
        Some("google-workspace.read"),
    );
    // Let all watchers poll at least once so r3's status is queued.
    assert!(
        rig.slack
            .until(|s| s
                .calls("chat.appendStream")
                .iter()
                .any(|c| c.body["chunks"][0]["title"] == "Running tool: notion.get"))
            .await
    );
    tokio::time::sleep(Duration::from_millis(60)).await;

    let snap = personal().build();

    // r2 (non-owner) finishes first: r1's ts1 must stay untouched.
    let (item2, body2) = thread_reply(&r2, 1, "200.1", "gmail reply");
    let _ = rig.gateway.sender().send(&snap, &item2, &body2).await;
    rig.store.set_progress(&r2, RunState::Succeeded);
    assert!(
        rig.slack
            .until(|s| s
                .calls("reactions.add")
                .iter()
                .any(|c| c.body["name"] == "white_check_mark"))
            .await
    );
    assert!(rig.slack.calls("chat.stopStream").is_empty());
    assert!(rig.slack.calls("chat.delete").is_empty());
    assert_eq!(rig.slack.calls("chat.startStream").len(), 1);

    // r1 finishes second: skips already-completed r2 and immediately opens r3's stream.
    let (item1, body1) = thread_reply(&r1, 2, "200.1", "notion reply");
    let _ = rig.gateway.sender().send(&snap, &item1, &body1).await;
    rig.store.set_progress(&r1, RunState::Succeeded);
    assert!(
        rig.slack
            .until(|s| s.calls("chat.startStream").len() == 2)
            .await
    );
    let start2 = rig.slack.calls("chat.startStream")[1].clone();
    assert_eq!(
        start2.body["chunks"][0]["title"],
        "Running tool: google-workspace.read"
    );
    assert_ne!(start2.reply["ts"], ts1);
    let ts2 = start2.reply["ts"].as_str().unwrap().to_owned();

    // r3 finishes last: closes ts2 with default active session_status.
    let (item3, body3) = thread_reply(&r3, 3, "200.1", "calendar reply");
    let _ = rig.gateway.sender().send(&snap, &item3, &body3).await;
    rig.store.set_progress(&r3, RunState::Succeeded);
    assert!(
        rig.slack
            .until(|s| s.calls("chat.delete").iter().any(|c| c.body["ts"] == ts2))
            .await
    );
    let stop2 = rig
        .slack
        .calls("chat.stopStream")
        .into_iter()
        .find(|c| c.body["ts"] == ts2)
        .expect("ts2 stopped");
    assert!(stop2.body.get("session_status").is_none());
    assert_eq!(rig.slack.calls("chat.startStream").len(), 2);
}

#[tokio::test]
async fn non_reply_cancel_hands_off_unchanged_status_and_shutdown_does_not_reopen() {
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
            "300.1",
            "<@UBOT> one",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.startStream").len() == 1)
            .await
    );
    rig.slack
        .push(thread_mention_frame(
            "E2",
            "C1",
            "U1",
            "300.2",
            "300.1",
            "<@UBOT> two",
        ))
        .await;
    rig.slack
        .push(thread_mention_frame(
            "E3",
            "C1",
            "U1",
            "300.3",
            "300.1",
            "<@UBOT> three",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("reactions.add").len() == 3)
            .await
    );
    tokio::time::sleep(Duration::from_millis(60)).await;

    let accepted = rig.store.inner.accepted();
    let (r1, r2) = (accepted[0].run.clone(), accepted[1].run.clone());

    // Cancel r1 without posting a reply; r2 (still in unchanged "Drafting reply") must take over immediately.
    rig.store.set_progress(&r1, RunState::Cancelled);
    assert!(
        rig.slack
            .until(|s| s.calls("chat.startStream").len() == 2)
            .await
    );
    let start2 = rig.slack.calls("chat.startStream")[1].clone();
    assert_eq!(start2.body["chunks"][0]["title"], "Drafting reply");

    // Graceful shutdown while r2 and r3 are still active must close r2's stream and never open r3's.
    rig.stop.cancel();
    assert!(
        rig.slack
            .until(|s| s.calls("chat.stopStream").len() == 2)
            .await
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(rig.slack.calls("chat.startStream").len(), 2);
    let stop2 = &rig.slack.calls("chat.stopStream")[1];
    assert!(
        stop2.body.get("session_status").is_none(),
        "graceful shutdown must not keep session_status=processing"
    );
    let _ = r2;
}

#[tokio::test]
async fn append_stream_rate_limit_keeps_stream_while_closed_error_deletes_and_reopens() {
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
            "400.1",
            "<@UBOT> long",
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
    let run = rig.store.inner.accepted()[0].run.clone();

    // 1) Transient 429 on appendStream keeps ts1 alive and retries appendStream on the next poll.
    rig.slack.script(
        "chat.appendStream",
        Scripted::Raw {
            status: 429,
            retry_after: Some(1),
            body: String::new(),
        },
    );
    rig.store.set_progress_detail(
        &run,
        RunState::Leased,
        Some("main/workspace"),
        Some("notion.search"),
    );
    assert!(
        rig.slack
            .until(|s| s
                .calls("chat.appendStream")
                .iter()
                .filter(|c| c.body["ts"] == ts1)
                .count()
                >= 2)
            .await
    );
    assert!(rig.slack.calls("chat.delete").is_empty());
    assert_eq!(rig.slack.calls("chat.startStream").len(), 1);

    // 2) Permanent Slack error (e.g. 10-minute stream expiry) deletes ts1 and reopens a fresh stream.
    rig.slack.script(
        "chat.appendStream",
        Scripted::Json(json!({"ok": false, "error": "message_not_in_streaming_state"})),
    );
    rig.store.set_progress_detail(
        &run,
        RunState::Leased,
        Some("main/workspace"),
        Some("notion.get"),
    );
    assert!(
        rig.slack
            .until(|s| s.calls("chat.startStream").len() == 2)
            .await
    );
    assert!(
        rig.slack
            .calls("chat.delete")
            .iter()
            .any(|c| c.body["ts"] == ts1)
    );
    let start2 = &rig.slack.calls("chat.startStream")[1];
    assert_eq!(
        start2.body["chunks"][0]["title"],
        "Running tool: notion.get"
    );
}
