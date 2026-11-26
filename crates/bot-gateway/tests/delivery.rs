//! 회신 발송 수락 시험: outbox claim → 현재 판본 확인 → begin → Slack 한 번 → settle. 숨은 재전송이 없고,
//! 결과는 Slack가 증명한 만큼만(확정·미실행·거절·불명) 정산한다.
use bot_core::{
    id::{ProfileId, RunId},
    intake::PushTransportId,
    port::Gateway,
    state::{Conversation, ConversationKind},
};
use bot_gateway::testing::{ports::outgoing, slack::Scripted, *};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

mod delivery_support;
mod support;
use delivery_support::*;
use support::{default_secrets, personal, secrets};

#[tokio::test]
async fn a_text_reply_goes_to_the_original_thread_once_and_is_confirmed() {
    let rig = rig().await;
    to_channel(&rig, 1, json!({"text": "일정은 3건이에요"}));
    assert_eq!(rig.deliverer.tick().await.unwrap(), 1);
    let calls = posts(&rig);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].token, "test-bot-1");
    let body = &calls[0].body;
    assert_eq!(
        (body["channel"].as_str(), body["thread_ts"].as_str()),
        (Some("C1"), Some(THREAD))
    );
    assert_eq!(body["blocks"][0]["type"], "markdown");
    assert_eq!(body["blocks"][0]["text"], "일정은 3건이에요");
    assert_eq!(body["text"], "일정은 3건이에요");
    assert_eq!(settled(&rig), ["confirmed:C1:1700000000.000002"]);
    // begin에는 현재 판본이 넘어갔다
    assert_eq!(rig.outbox.begun()[0].1, "personal-r1");
    // 같은 예약은 다시 잡히지 않는다
    assert_eq!(rig.deliverer.tick().await.unwrap(), 0);
    assert_eq!(posts(&rig).len(), 1);
}

#[tokio::test]
async fn dm_replies_use_the_dm_channel_and_broadcast_mentions_are_neutralised() {
    let rig = rig().await;
    rig.outbox.push(outgoing(
        "personal",
        1,
        "direct",
        "slack:home/U1",
        direct("D1"),
        &json!({"text": "<!channel> 안녕"}).to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    let body = &posts(&rig)[0].body;
    assert_eq!(body["channel"], "D1");
    assert_eq!(body["blocks"][0]["text"], "&lt;!channel> 안녕");
    assert_eq!(body["text"], "&lt;!channel> 안녕");
}

#[tokio::test]
async fn approval_cards_carry_buttons_and_a_card_that_would_be_cut_does_not() {
    let rig = rig().await;
    let card = json!({"nonce": "n-1", "title": "일정 삭제", "lines": ["대상: 산책"]});
    to_channel(
        &rig,
        1,
        json!({"text": "승인이 필요해요", "approval": card}),
    );
    rig.deliverer.tick().await.unwrap();
    let blocks = posts(&rig)[0].body["blocks"].as_array().unwrap().clone();
    let types: Vec<_> = blocks.iter().map(|b| b["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["markdown", "header", "section", "actions"]);
    let buttons = blocks[3]["elements"].as_array().unwrap();
    assert_eq!(buttons[0]["action_id"], "bot_approve");
    assert_eq!(buttons[1]["action_id"], "bot_deny");
    assert!(buttons.iter().all(|b| b["value"] == "n-1"));

    // 잘라야 하는 카드는 버튼 없이 관리 화면으로 안내한다
    let long = json!({"nonce": "n-2", "title": "큰 변경", "lines": vec!["가".repeat(500); 7]});
    to_channel(&rig, 2, json!({"approval": long}));
    rig.deliverer.tick().await.unwrap();
    let second = &posts(&rig)[1].body;
    assert!(
        second["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["type"] != "actions")
    );
    assert!(
        second["text"]
            .as_str()
            .unwrap()
            .contains("https://admin.example/approvals")
    );
    assert_eq!(settled(&rig).len(), 2);
}

#[tokio::test]
async fn outcomes_are_settled_by_what_slack_proved_and_nothing_is_resent() {
    for (script, want) in [
        (
            Scripted::Raw {
                status: 429,
                retry_after: Some(7),
                body: String::new(),
            },
            "notsent:RateLimited:7",
        ),
        (
            Scripted::Json(json!({"ok": false, "error": "channel_not_found"})),
            "rejected",
        ),
        (
            Scripted::Json(json!({"ok": false, "error": "internal_error"})),
            "held",
        ),
        (
            Scripted::Raw {
                status: 500,
                retry_after: None,
                body: "oops".into(),
            },
            "held",
        ),
        (
            Scripted::Raw {
                status: 502,
                retry_after: None,
                body: r#"{"ok":true,"ts":"1.1"}"#.into(),
            },
            "held",
        ),
        (Scripted::Json(json!({"ok": true})), "held"),
    ] {
        let rig = rig().await;
        rig.slack.script("chat.postMessage", script);
        to_channel(&rig, 1, json!({"text": "안녕"}));
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig), [want]);
        assert_eq!(posts(&rig).len(), 1, "{want}: 숨은 재전송이 없어야 해요");
    }
}

#[tokio::test]
async fn a_refused_connection_is_not_sent() {
    let rig = rig_with(
        personal(),
        default_secrets(),
        Some("http://127.0.0.1:1/api/"),
    )
    .await;
    to_channel(&rig, 1, json!({"text": "안녕"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["notsent:BeforeWrite:0"]);
}

#[tokio::test]
async fn a_missing_bot_token_sends_nothing() {
    let rig = rig_with(personal(), secrets(&[("personal", "app-token", "x")]), None).await;
    to_channel(&rig, 1, json!({"text": "안녕"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["notsent:BeforeWrite:0"]);
    assert!(rig.slack.all_calls().is_empty());
}

#[tokio::test]
async fn a_format_rejection_falls_back_to_plain_text_once_but_never_for_cards() {
    let rig = rig().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_blocks"})),
    );
    to_channel(&rig, 1, json!({"text": "표가 깨졌어요"}));
    rig.deliverer.tick().await.unwrap();
    let calls = posts(&rig);
    assert_eq!(calls.len(), 2);
    assert!(calls[0].body.get("blocks").is_some());
    assert!(calls[1].body.get("blocks").is_none());
    assert_eq!(calls[1].body["text"], "표가 깨졌어요");
    assert_eq!(settled(&rig), ["confirmed:C1:1700000000.000002"]);

    // 승인 카드는 버튼이 없으면 의미가 없으니 글자판으로 내리지 않고 거절한다
    let rig = self::rig().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_blocks"})),
    );
    to_channel(
        &rig,
        1,
        json!({"approval": {"nonce": "n", "title": "t", "lines": ["x"]}}),
    );
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 1);
    assert_eq!(settled(&rig), ["rejected"]);
}

#[tokio::test]
async fn replies_outside_the_original_conversation_are_rejected_before_any_slack_call() {
    let rig = rig().await;
    let ok = json!({"text": "안녕"}).to_string();
    let bad_thread = Conversation {
        thread: Some("not-a-ts".into()),
        ..channel("C1")
    };
    rig.outbox.push(outgoing(
        "personal",
        1,
        "team",
        "slack:home/U1",
        channel("C9"),
        &ok,
    )); // 다른 채널
    rig.outbox.push(outgoing(
        "personal",
        2,
        "team",
        "slack:home/U1",
        direct("D1"),
        &ok,
    )); // 채널 바인딩에 DM
    rig.outbox.push(outgoing(
        "personal",
        3,
        "direct",
        "slack:home/U1",
        channel("C1"),
        &ok,
    )); // DM 바인딩에 채널
    rig.outbox.push(outgoing(
        "personal",
        4,
        "ghost",
        "slack:home/U1",
        channel("C1"),
        &ok,
    )); // 없는 바인딩
    rig.outbox.push(outgoing(
        "personal",
        5,
        "team",
        "slack:home/U9",
        channel("C1"),
        &ok,
    )); // 멤버가 아님
    rig.outbox.push(outgoing(
        "personal",
        6,
        "team",
        "slack:home/U1",
        bad_thread,
        &ok,
    )); // 스레드 모양
    rig.outbox.push(outgoing(
        "personal",
        7,
        "team",
        "slack:home/U1",
        channel("C1"),
        r#"{"text":"x","html":"<b>"}"#,
    ));
    rig.outbox.push(outgoing(
        "personal",
        8,
        "team",
        "slack:home/U1",
        channel("C1"),
        "not json",
    ));
    assert_eq!(rig.deliverer.tick().await.unwrap(), 8);
    assert_eq!(settled(&rig), ["rejected"; 8]);
    assert_eq!(
        rig.outbox.begun().len(),
        8,
        "거절도 begin으로 점유를 기록한 뒤 정산한다"
    );
    assert!(rig.slack.all_calls().is_empty(), "보내지 않았어요");
}

#[tokio::test]
async fn a_failed_begin_sends_nothing_and_settles_nothing() {
    let rig = rig().await;
    rig.outbox.fail_begin(true);
    to_channel(&rig, 1, json!({"text": "안녕"}));
    rig.deliverer.tick().await.unwrap();
    assert!(rig.slack.all_calls().is_empty());
    assert!(settled(&rig).is_empty());
}

#[tokio::test]
async fn an_inactive_profile_is_left_alone_and_the_current_revision_is_checked() {
    let rig = rig().await;
    rig.outbox.push(outgoing(
        "ghost",
        1,
        "team",
        "slack:home/U1",
        channel("C1"),
        r#"{"text":"x"}"#,
    ));
    rig.deliverer.tick().await.unwrap();
    assert!(rig.outbox.begun().is_empty() && settled(&rig).is_empty());
    // 판본이 바뀌면 begin에는 새 판본이 넘어간다
    rig.catalog
        .replace([personal().revision("personal-r2").build()]);
    to_channel(&rig, 2, json!({"text": "안녕"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(rig.outbox.begun()[0].1, "personal-r2");
    // 주체가 멤버에서 빠지면 보내지 않는다
    rig.catalog.replace([profile("personal")
        .binding(
            "team",
            bot_core::profile::Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .build()]);
    to_channel(&rig, 3, json!({"text": "다시"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 1);
    assert_eq!(settled(&rig).last().map(String::as_str), Some("rejected"));
}

#[tokio::test]
async fn attachments_use_the_upload_flow_and_only_the_last_call_publishes() {
    let rig = rig().await;
    to_channel(
        &rig,
        1,
        json!({"text": "보고서예요", "attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]}),
    );
    rig.deliverer.tick().await.unwrap();
    let methods: Vec<_> = rig
        .slack
        .all_calls()
        .iter()
        .map(|c| c.method.clone())
        .collect();
    assert_eq!(
        methods,
        [
            "auth.test",
            "files.getUploadURLExternal",
            "files.completeUploadExternal"
        ]
    );
    assert_eq!(rig.slack.uploads(), [b"hi".to_vec()]);
    let slot = &rig.slack.calls("files.getUploadURLExternal")[0].body;
    assert_eq!(
        (slot["filename"].as_str(), slot["length"].as_str()),
        (Some("a.txt"), Some("2"))
    );
    let done = &rig.slack.calls("files.completeUploadExternal")[0].body;
    assert_eq!(done["channel_id"], "C1");
    assert_eq!(done["thread_ts"], THREAD);
    assert_eq!(done["initial_comment"], "보고서예요");
    assert!(
        done["files"]
            .as_str()
            .unwrap()
            .contains("\"title\":\"a.txt\"")
    );
    assert!(settled(&rig)[0].starts_with("confirmed:C1:F"));

    // 완료 호출의 결과 분류: 429는 미실행, ok:false는 거절, 5xx는 불명
    for (script, want) in [
        (
            Scripted::Raw {
                status: 429,
                retry_after: Some(3),
                body: String::new(),
            },
            "notsent:RateLimited:3",
        ),
        (
            Scripted::Json(json!({"ok": false, "error": "channel_not_found"})),
            "rejected",
        ),
        (
            Scripted::Raw {
                status: 500,
                retry_after: None,
                body: String::new(),
            },
            "held",
        ),
    ] {
        let rig = self::rig().await;
        rig.slack.script("files.completeUploadExternal", script);
        to_channel(
            &rig,
            1,
            json!({"attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]}),
        );
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig), [want]);
        assert_eq!(
            rig.slack.calls("files.completeUploadExternal").len(),
            1,
            "{want}: 다시 보내지 않는다"
        );
    }
    // 주소 받기가 막히면 아무것도 게시되지 않았으니 미실행
    let rig = self::rig().await;
    rig.slack.script(
        "files.getUploadURLExternal",
        Scripted::Raw {
            status: 429,
            retry_after: Some(5),
            body: String::new(),
        },
    );
    to_channel(
        &rig,
        1,
        json!({"attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]}),
    );
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["notsent:RateLimited:5"]);
    assert!(rig.slack.calls("files.completeUploadExternal").is_empty());
    // 승인 카드와 첨부는 한 회신에 함께 보내지 않는다
    let rig = self::rig().await;
    to_channel(
        &rig,
        1,
        json!({"attachments": [{"name": "a", "type": "a/b", "data": ""}], "approval": {"nonce": "n", "title": "t", "lines": []}}),
    );
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["rejected"]);
    assert!(rig.slack.all_calls().is_empty());
}

#[tokio::test]
async fn split_inline_attachments_post_markdown_first_and_upload_files_to_the_same_thread() {
    let rig = rig().await;
    let raw = "**결론**이에요.\n\n※ 상세는 첨부 파일에 있어요.\n\n<첨부 이름=\"근거.md\">\n## 표\n| 항목 | 값 |\n|---|---|\n| A | 1 |\n</첨부>";
    let (body, files) = bot_core::attachments::split(raw);
    let (packed, truncated) = bot_core::attachments::pack_answers(&files, 15);
    assert!(!truncated);
    assert_eq!(packed.len(), 1);

    to_channel(&rig, 1, json!({"text": body}));
    to_channel(
        &rig,
        2,
        serde_json::from_str::<serde_json::Value>(&packed[0].0).unwrap(),
    );
    assert_eq!(rig.deliverer.tick().await.unwrap(), 2);

    // 첫 번째 회신은 마크다운 본문(`chat.postMessage`)으로 올라가고 `<첨부>` 원문이 남지 않는다
    let posted = posts(&rig);
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].body["thread_ts"], THREAD);
    assert_eq!(posted[0].body["blocks"][0]["type"], "markdown");
    let md = posted[0].body["blocks"][0]["text"].as_str().unwrap();
    assert_eq!(md, "**결론**이에요.\n\n※ 상세는 첨부 파일에 있어요.");
    assert!(!md.contains("<첨부"));

    // 두 번째 회신은 같은 스레드에 별도 파일(`근거.md`)로 올라간다(`initial_comment` 없음)
    let slot = &rig.slack.calls("files.getUploadURLExternal")[0].body;
    let expected = "## 표\n| 항목 | 값 |\n|---|---|\n| A | 1 |";
    assert_eq!(slot["filename"], "근거.md");
    assert_eq!(slot["length"], expected.len().to_string());
    assert_eq!(rig.slack.uploads(), [expected.as_bytes().to_vec()]);
    let done = &rig.slack.calls("files.completeUploadExternal")[0].body;
    assert_eq!(done["channel_id"], "C1");
    assert_eq!(done["thread_ts"], THREAD);
    assert!(done.get("initial_comment").is_none());
    assert!(
        done["files"]
            .as_str()
            .unwrap()
            .contains("\"title\":\"근거.md\"")
    );
    assert_eq!(settled(&rig).len(), 2);
    assert!(settled(&rig).iter().all(|s| s.starts_with("confirmed:C1:")));
}

#[tokio::test]
async fn slack_does_not_claim_another_transport_in_the_same_kind_of_conversation() {
    let rig = rig().await;
    let mut foreign = outgoing(
        "personal",
        1,
        "team",
        "slack:home/U1",
        Conversation {
            kind: ConversationKind::Channel,
            id: "C1".into(),
            thread: Some(THREAD.into()),
        },
        r#"{"text":"다른 발송자의 회신"}"#,
    );
    foreign.transport = PushTransportId::new("fixture-push").unwrap();
    rig.outbox.push(foreign);
    assert_eq!(rig.deliverer.tick().await.unwrap(), 0);
    assert!(rig.outbox.begun().is_empty());
    assert!(rig.slack.all_calls().is_empty() && settled(&rig).is_empty());
}

#[tokio::test]
async fn the_run_loop_delivers_and_a_deliver_signal_wakes_it() {
    let rig = rig().await;
    let stop = CancellationToken::new();
    let runner = {
        let deliverer = rig.deliverer.clone();
        let stop = stop.clone();
        tokio::spawn(async move { deliverer.run(stop).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    to_channel(&rig, 1, json!({"text": "안녕"}));
    rig.deliverer
        .deliver(RunId("run-1".into()), ProfileId("personal".into()))
        .await
        .unwrap();
    for _ in 0..200 {
        if !settled(&rig).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(settled(&rig).len(), 1);
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap();
}
