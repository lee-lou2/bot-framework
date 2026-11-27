//! `SlackGateway::with_ux` 통합 시험: 권한 철회·빈 사용자·구판 프로세스·취소 일시 실패·겹치는 실행·감시 만료.
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
            "<@UBOT> 일 시작",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E1".into())).await);
    assert!(
        rig.slack
            .until(|s| !s.calls("chat.startStream").is_empty())
            .await
    );

    // U1의 멤버십을 철회하고 U2(admin)만 남긴다
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

    // 철회된 본인(U1)이 중단 버튼·중단 멘션을 보내도 거절된다
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
            "<@UBOT> 중단해줘",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 3).await);
    assert!(
        rig.admin.cancels().is_empty(),
        "철회된 요청자는 중단할 수 없어요"
    );
    assert!(
        rig.slack.calls("chat.postMessage").is_empty(),
        "확인 카드도 올리지 않아요"
    );
    assert!(
        !rig.slack.calls("chat.postEphemeral").is_empty(),
        "권한 거절 안내가 간다"
    );

    // 활성 멤버인 admin(U2)은 중단할 수 있다
    rig.slack
        .push(thread_mention_frame(
            "E4",
            "C1",
            "U2",
            "10.3",
            "10.1",
            "<@UBOT> 중단해줘",
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
            "<@UBOT> 작업",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E1".into())).await);
    assert!(
        rig.slack
            .until(|s| !s.calls("reactions.add").is_empty())
            .await
    );

    // 소켓 프레임: user 누락·빈 문자열 모두 무시된다
    rig.slack
        .push(stop_requested_frame("E2", "C1", "20.1", None))
        .await;
    rig.slack
        .push(stop_requested_frame("E3", "C1", "20.1", Some("")))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 3).await);

    // 직접 on_frame으로 빈 user를 넘겨도 닫힌 쪽으로 무시한다
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
            "<@UBOT> 작업",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E1".into())).await);
    assert!(
        rig.slack
            .until(|s| !s.calls("reactions.add").is_empty())
            .await
    );

    // 먼저 정상 상태에서 확인 카드를 띄워 둔다
    rig.slack
        .push(stop_requested_frame("E2", "C1", "30.1", Some("U1")))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E2".into())).await);
    let card_ts = rig.slack.calls("chat.postMessage")[0].reply["ts"]
        .as_str()
        .unwrap()
        .to_owned();

    // 다른 프로세스가 새 집합을 활성화해 이 프로세스가 구판이 됐다
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
            "<@UBOT> 중단해줘",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 4).await);
    assert!(
        rig.admin.cancels().is_empty(),
        "구판은 cancel을 부르지 않는다"
    );
    assert!(
        rig.slack.calls("chat.update").is_empty(),
        "구판은 확인 카드를 STOP_ACK나 STOP_GONE으로 덮어쓰지 않는다"
    );
    assert_eq!(
        rig.store.inner.accepted().len(),
        1,
        "구판 중단 멘션이 일반 접수로 흘러가지 않는다"
    );

    // ActiveCheck가 Unavailable이면 ack하지 않는다(`false`)
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
            "<@UBOT> 작업",
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

    // 첫 cancel이 일시 장애(Unavailable)로 실패한다: ack하지 않고 STOP_ACK도 쓰지 않는다
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
        "실패한 cancel은 ack하지 않아 Slack이 재전달해야 해요"
    );
    assert!(
        rig.slack.calls("chat.update").is_empty(),
        "실패한 cancel에 STOP_ACK를 올리면 안 돼요"
    );

    // 같은 버튼을 동시에 두 번 누르면 cancel은 한 번만 나가고 STOP_GONE도 뜨지 않는다
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
        "동시 클릭이 cancel을 두 번 부르지 않아요"
    );
    let updates = rig.slack.calls("chat.update");
    assert_eq!(updates.len(), 1);
    assert!(
        updates[0].body["text"]
            .as_str()
            .unwrap()
            .contains("중단을 요청하셨어요"),
        "{:?}",
        updates[0].body
    );
    // STOP_CONTINUE 버튼은 이미 확정된 run에 STOP_GONE을 덮어쓰지 않는다
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
    let rig = ux_rig(vec![personal()], None, None).await;
    // 같은 스레드(C1, 50.1)에서 첫 번째 run 접수 → 스트림 ts1 열림
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "50.1",
            "<@UBOT> 첫 요청",
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

    // 같은 스레드에서 두 번째 run 접수 → 스트림 ts2 열림
    rig.slack
        .push(thread_mention_frame(
            "E2",
            "C1",
            "U1",
            "50.2",
            "50.1",
            "<@UBOT> 둘째 요청",
        ))
        .await;
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

    let accepted = rig.store.inner.accepted();
    assert_eq!(accepted.len(), 2);
    let run1 = accepted[0].run.clone();
    let run2 = accepted[1].run.clone();

    // 첫 번째 run의 최종 회신을 보낸다(clear_stream에 run1 전달): run2의 살아 있는 ts2를 닫거나 지우면 안 된다
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
        r#"{"text":"첫 답"}"#,
    );
    item.key.run = run1.clone();
    let body = ReplyBody {
        text: Some("첫 답".into()),
        approval: None,
        attachments: vec![],
    };
    let _ = rig.gateway.sender().send(&snap, &item, &body).await;
    // run1을 종료시켜도 run2의 스트림(ts2)과 활성 추적은 그대로 남는다
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
        "run1 완료가 run2의 살아 있는 스트림을 닫았어요"
    );
    assert!(
        rig.slack
            .calls("chat.delete")
            .iter()
            .all(|c| c.body["ts"] != ts2),
        "run1 완료가 run2의 살아 있는 스트림을 지웠어요"
    );
    // run2는 여전히 스레드에서 추적 중이라 중단할 수 있다
    rig.slack
        .push(thread_mention_frame(
            "E3",
            "C1",
            "U1",
            "50.3",
            "50.1",
            "<@UBOT> 중단해줘",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().contains(&"E3".into())).await);
    let cancels = rig.admin.cancels();
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].1, run2);
}

#[tokio::test]
async fn watch_timeout_and_graceful_shutdown_exit_cleanly_without_reacting_x() {
    // 1) 감시 기한 만료: 작업이 아직 Leased여도 `:x:`를 달지 않는다
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
            "<@UBOT> 긴 작업",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| !s.calls("chat.startStream").is_empty())
            .await
    );
    // 기한(120ms) 만료 후 스트림이 정리될 때까지 기다린다
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
    assert_eq!(added, ["eyes"], "만료된 감시가 :x:를 달면 안 돼요");
    assert!(rig.slack.calls("reactions.remove").is_empty());

    // 2) 정상 종료(CancellationToken): 진행 중 작업에 `:x:`를 달지 않는다
    let rig2 = ux_rig(vec![personal()], None, None).await;
    rig2.slack
        .push(mention_frame(
            "E2",
            "T1",
            "C1",
            "U1",
            "61.1",
            "<@UBOT> 종료 중 작업",
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
    assert_eq!(added2, ["eyes"], "정상 종료가 :x:를 달면 안 돼요");
    assert!(rig2.slack.calls("reactions.remove").is_empty());
}
