//! Slack 이벤트 선택(메시지) 수신 수락 시험: 바인딩의 `events`에 따라 멘션·스레드 답글을 접수한다(가짜 Slack에 소켓으로 붙어 인증된 설치 → 접수 → ack).
//! 기본 설정 회귀, 봇이 참여한 스레드의 증거(두 경로), 이웃 스레드·미등록 채널·봇·편집 제외, 멘션/메시지 두 구독의 양 순서와 재전달, 증거를 읽지 못할 때의 재전달.
//! 반응·넘김 후보 사슬은 `slack_reactions.rs`.
use bot_core::slack::{SlackEvent, SourceKind};
use bot_gateway::testing::slack::{mention_frame, post_frame, reaction_frame};
use serde_json::{Value, json};
mod support;
use support::{events::*, *};

#[tokio::test]
async fn without_events_only_mentions_and_dms_work_as_before() {
    let rig = connected(vec![channel_profile(&[SlackEvent::Mention])]).await;
    rig.threads.seed(&thread("100.1"));
    push_all(
        &rig,
        vec![
            post_frame("E1", TC1, "U1", "100.2", Some("100.1"), "스레드 답글"),
            reaction_frame("E2", TC1, "U1", "100.1", "white_check_mark", "U9"),
            post_frame("E3", TC1, "U1", "101.1", None, "그냥 채널 글"),
            mention_frame("E4", "T1", "C1", "U1", "102.1", "<@UBOT> 안녕"),
        ],
    )
    .await;
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].target, "agent main 안녕");
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
            post_frame("E1", TC1, "U1", "100.2", Some("100.1"), "이어서 알려줘"),
            // 이웃 스레드·최상위 글·등록 안 된 사람·등록 안 된 채널은 접수하지 않는다(받았다는 표시만)
            post_frame("E2", TC1, "U1", "200.2", Some("200.1"), "이웃 스레드"),
            post_frame("E3", TC1, "U1", "300.1", None, "최상위 글"),
            post_frame("E4", TC1, "U9", "100.3", Some("100.1"), "등록 안 된 사람"),
            post_frame(
                "E5",
                ("T1", "C9"),
                "U1",
                "100.4",
                Some("100.1"),
                "다른 채널",
            ),
            post_frame("E6", ("T9", "C1"), "U1", "100.5", Some("100.1"), "다른 팀"),
            // 수정·삭제·앱 글·잘못된 ts
            event(
                "E7",
                json!({"type": "message", "subtype": "message_changed", "channel_type": "channel",
                "user": "U1", "ts": "100.6", "thread_ts": "100.1", "channel": "C1"}),
            ),
            event(
                "E8",
                json!({"type": "message", "channel_type": "channel", "user": "U7", "bot_id": "B9",
                "text": "다른 앱", "ts": "100.7", "thread_ts": "100.1", "channel": "C1"}),
            ),
            post_frame("E9", TC1, "U1", "not-a-ts", Some("100.1"), "잘못된 ts"),
        ],
    )
    .await;
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1, "{records:?}");
    let r = &records[0];
    assert_eq!(r.target, "agent main 이어서 알려줘");
    assert_eq!(r.key.request.0, "C1.100.2");
    assert_eq!(r.conversation, "channel:C1:100.1");
    let body: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(
        body,
        json!({"text": "이어서 알려줘", "channel": "C1", "ts": "100.2", "thread_ts": "100.1"})
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
    // 같은 글의 재전달은 한 요청이다
    push_all(
        &rig,
        vec![post_frame(
            "E1b",
            TC1,
            "U1",
            "100.2",
            Some("100.1"),
            "이어서 알려줘",
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
            "text": "봇 글", "ts": ts, "channel": "C1"});
        if let Some(root) = root {
            e["thread_ts"] = json!(root);
        }
        event(envelope, e)
    };
    push_all(
        &rig,
        vec![
            // 봇이 최상위 글을 올렸다(봇 루트) · 사람 글 아래에 답했다(사람 루트) · 다른 앱의 글
            own("E1", "500.1", None),
            own("E2", "600.2", Some("600.1")),
            event("E3", json!({"type": "message", "channel_type": "channel", "user": "UOTHER", "bot_id": "B9",
                "text": "다른 앱", "ts": "700.1", "channel": "C1"})),
        ],
    )
    .await;
    // 봇 글을 관찰해도 트리거는 실행하지 않는다(접수 0)
    assert!(rig.store.accepted().is_empty());
    let roots: Vec<String> = rig.threads.notes().into_iter().map(|n| n.root_ts).collect();
    assert_eq!(roots, ["500.1", "600.1"]);
    push_all(
        &rig,
        vec![
            post_frame("E4", TC1, "U1", "500.2", Some("500.1"), "봇 루트에 답"),
            post_frame("E5", TC1, "U3", "600.3", Some("600.1"), "사람 루트에 답"),
            post_frame("E6", TC1, "U1", "700.2", Some("700.1"), "다른 앱 글 아래"),
        ],
    )
    .await;
    let targets: Vec<String> = rig.store.accepted().into_iter().map(|r| r.target).collect();
    assert_eq!(targets.len(), 2, "{targets:?}");
    assert!(targets.contains(&"agent main 봇 루트에 답".to_owned()));
    assert!(targets.contains(&"agent main 사람 루트에 답".to_owned()));

    // `thread_reply`를 켜지 않은 바인딩은 자기 봇 글도 기록하지 않는다
    let plain = connected(vec![channel_profile(&[SlackEvent::Mention])]).await;
    push_all(&plain, vec![own("E1", "500.1", None)]).await;
    assert!(plain.threads.notes().is_empty());
}

#[tokio::test]
async fn one_post_delivered_as_mention_and_message_is_one_request_in_either_order() {
    for (events, want_reply) in [(&ALL[..2], false), (&[SlackEvent::ThreadReply][..], true)] {
        let rig = connected(vec![channel_profile(events)]).await;
        rig.threads.seed(&thread("100.1"));
        let text = "<@UBOT> 확인해 줘";
        // 순서 A: app_mention 먼저 · 순서 B: message 먼저. 다른 이벤트 ID로 같은 글이 두 번 온다
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
            // 멘션이 켜져 있으면 멘션이 우선이고, 아니면 스레드 답글이다 — 도착 순서와 무관하다
            assert_eq!(
                source.kind,
                if want_reply {
                    SourceKind::ThreadReply
                } else {
                    SourceKind::Message
                },
                "{events:?}"
            );
            assert_eq!(record.target, "agent main 확인해 줘");
        }
        // 멘션 우선: 접수한 글은 `{"text": …}`만 담는다(스레드 답글 필드가 없다)
        if !want_reply {
            let body: Value = serde_json::from_str(&records[0].body).unwrap();
            assert_eq!(body, json!({"text": "확인해 줘"}));
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
            "장애 중",
        ))
        .await;
    rig.slack
        .push(mention_frame(
            "E2",
            "T1",
            "C1",
            "U1",
            "101.1",
            "<@UBOT> 멘션",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"E2".to_owned()))
            .await
    );
    assert!(
        !rig.slack.acks().contains(&"E1".to_owned()),
        "읽지 못한 답글은 ack하지 않는다"
    );
    assert_eq!(rig.store.accepted().len(), 1, "멘션은 증거가 필요 없다");
    // 저장소가 돌아온 뒤 Slack이 같은 이벤트를 다시 보내면 접수한다
    rig.threads.fail_reads(false);
    rig.slack
        .push(post_frame(
            "E1",
            TC1,
            "U1",
            "100.2",
            Some("100.1"),
            "장애 중",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"E1".to_owned()))
            .await
    );
    assert_eq!(rig.store.accepted().len(), 2);
}
