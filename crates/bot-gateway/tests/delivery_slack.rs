//! Slack 발송의 정확성(외부로 나가는 글): ① 보관소의 봇 토큰을 바꿔도 등록한 팀의 것으로 확인된 뒤에만 게시한다, ② 본문을 자르지 않고 다 보낼 수 없으면
//! 완료로 기록하지 않는다, ③ 영수증은 Slack의 응답이 말한 채널·`ts`·파일로만 확인하고 확인할 수 없으면 불명(Held)이다.
mod delivery_support;
mod support;

use bot_core::ledger::ApprovalState;
use bot_gateway::testing::{
    ports::outgoing,
    slack::{Scripted, button_frame},
};
use delivery_support::*;
use serde_json::{Value, json};
use std::time::Duration;
use support::default_secrets;

const TTL: Duration = Duration::from_millis(40);

async fn wait_out_the_cache() {
    tokio::time::sleep(TTL * 2).await;
}

/// 새 조립(지역 변수 `rig`가 함수 `rig`를 가리지 않게 따로 부른다)
async fn fresh() -> Rig {
    rig().await
}

fn tokens(rig: &Rig) -> Vec<String> {
    posts(rig).into_iter().map(|call| call.token).collect()
}

#[tokio::test]
async fn a_rotated_bot_token_posts_only_after_it_is_checked_against_the_registered_team() {
    let secrets = default_secrets();
    let rig = rig_ttl(secrets.clone(), TTL).await;
    to_channel(&rig, 1, json!({"text": "하나"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(tokens(&rig), ["test-bot-1"]);
    assert_eq!(
        rig.slack.calls("auth.test").len(),
        1,
        "처음 쓰는 토큰은 확인한다"
    );

    // 같은 값이면 기억이 끝나도 확인을 되풀이하지 않는다
    wait_out_the_cache().await;
    to_channel(&rig, 2, json!({"text": "둘"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(tokens(&rig), ["test-bot-1", "test-bot-1"]);
    assert_eq!(rig.slack.calls("auth.test").len(), 1);

    // 보관소의 토큰이 다른 팀의 것으로 바뀌었다: 새 값을 확인하지 못했으니 게시하지 않는다(보내지 않았다 — 미실행)
    secrets.set("personal", "bot-token", b"other-team-token");
    rig.slack.set_auth("T9", "UBOT");
    wait_out_the_cache().await;
    to_channel(&rig, 3, json!({"text": "셋"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 2, "다른 팀의 토큰으로 게시하지 않았어요");
    assert_eq!(settled(&rig).last().unwrap(), "notsent:BeforeWrite:0");
    let checks = rig.slack.calls("auth.test");
    assert_eq!(
        (checks.len(), checks[1].token.as_str()),
        (2, "other-team-token")
    );

    // Slack에 닿지 않아 확인하지 못해도 같다(확인 없이 쓰지 않는다)
    secrets.set("personal", "bot-token", b"unchecked-token");
    rig.slack.script(
        "auth.test",
        Scripted::Json(json!({"ok": false, "error": "internal_error"})),
    );
    wait_out_the_cache().await;
    to_channel(&rig, 4, json!({"text": "넷"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 2);
    assert_eq!(settled(&rig).last().unwrap(), "notsent:BeforeWrite:0");

    // 등록한 팀의 새 토큰이면 확인한 뒤 그 값으로 게시한다
    secrets.set("personal", "bot-token", b"rotated-token");
    rig.slack.set_auth("T1", "UBOT");
    wait_out_the_cache().await;
    to_channel(&rig, 5, json!({"text": "다섯"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(tokens(&rig), ["test-bot-1", "test-bot-1", "rotated-token"]);
    assert_eq!(rig.slack.calls("auth.test").len(), 4);
}

#[tokio::test]
async fn a_body_is_sent_whole_or_not_completed() {
    // 형식 거절 뒤 글자판으로 내려도 전체 본문이다(3,000자에서 자르고 완료로 기록하지 않는다)
    let rig = fresh().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_blocks"})),
    );
    let text = "가".repeat(3_104);
    to_channel(&rig, 1, json!({ "text": text }));
    rig.deliverer.tick().await.unwrap();
    let calls = posts(&rig);
    assert_eq!(calls.len(), 2);
    assert!(calls[1].body.get("blocks").is_none());
    assert_eq!(calls[1].body["text"].as_str().unwrap(), text);
    assert!(settled(&rig)[0].starts_with("confirmed:C1:"));

    // markdown 블록 한도(11,000자) 안의 본문은 블록 하나에 잘림 없이 담긴다
    let rig = fresh().await;
    let text = "나".repeat(11_000);
    to_channel(&rig, 1, json!({ "text": text }));
    rig.deliverer.tick().await.unwrap();
    let calls = posts(&rig);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["blocks"][0]["text"].as_str().unwrap(), text);

    // 블록 한도를 넘는 본문(답 한도 12,000자 안)은 자르지 않고 글자판 한 메시지로 전체를 보낸다
    let rig = fresh().await;
    let text = "다".repeat(11_800);
    to_channel(&rig, 1, json!({ "text": text }));
    rig.deliverer.tick().await.unwrap();
    let calls = posts(&rig);
    assert_eq!(calls.len(), 1);
    assert!(calls[0].body.get("blocks").is_none());
    assert_eq!(calls[0].body["text"].as_str().unwrap(), text);
    assert!(settled(&rig)[0].starts_with("confirmed:C1:"));

    // 알림 멘션을 글자 그대로 바꾸면 길어져 블록 한도를 넘는 본문도 바꾼 글 전체를 글자판으로 보낸다
    let rig = fresh().await;
    to_channel(&rig, 1, json!({ "text": "<!here>".repeat(1_700) }));
    rig.deliverer.tick().await.unwrap();
    let calls = posts(&rig);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].body["text"].as_str().unwrap(),
        "&lt;!here>".repeat(1_700)
    );

    // 승인 카드와 함께는 담을 수 없는 긴 본문은 보내지 않는다(완료로 기록하지 않는다)
    let rig = fresh().await;
    to_channel(
        &rig,
        1,
        json!({"text": "라".repeat(11_800), "approval": {"nonce": "n", "title": "t", "lines": ["x"]}}),
    );
    rig.deliverer.tick().await.unwrap();
    assert!(rig.slack.all_calls().is_empty());
    assert_eq!(settled(&rig), ["rejected"]);
}

/// 응답을 이렇게 꾸미면 정산이 어떻게 되는가
async fn settled_for(reply: Value) -> String {
    let rig = fresh().await;
    rig.slack.script("chat.postMessage", Scripted::Json(reply));
    to_channel(&rig, 1, json!({"text": "안녕"}));
    rig.deliverer.tick().await.unwrap();
    settled(&rig).remove(0)
}

#[tokio::test]
async fn a_receipt_is_only_what_the_slack_response_confirms() {
    // 응답이 다른 채널을 말하거나 채널·`ts`를 말하지 않거나 `ts`가 시각 모양이 아니면 올라간 곳을 확인할 수 없다 → 불명
    for reply in [
        json!({"ok": true, "channel": "C2", "ts": "1700000000.000009"}),
        json!({"ok": true, "ts": "1700000000.000009"}),
        json!({"ok": true, "channel": "C1"}),
        json!({"ok": true, "channel": "C1", "ts": "later"}),
        json!({"ok": true, "channel": ["C1"], "ts": "1700000000.000009"}),
    ] {
        assert_eq!(settled_for(reply.clone()).await, "held", "{reply}");
    }
    assert_eq!(
        settled_for(json!({"ok": true, "channel": "C1", "ts": "1700000000.000009"})).await,
        "confirmed:C1:1700000000.000009"
    );

    // 성공 응답이 «메시지가 잘렸다»고 경고하면 이미 일부가 게시됐으니 자동 재전송 없이 불명으로 둔다. 관련 없는 경고는 정상 완료를 막지 않는다
    let truncated = json!({"ok": true, "channel": "C1", "ts": "1700000000.000009",
        "response_metadata": {"warnings": ["missing_charset", "message_truncated"]}});
    assert_eq!(settled_for(truncated).await, "held");
    let unrelated = json!({"ok": true, "channel": "C1", "ts": "1700000000.000009",
        "warning": "missing_charset", "response_metadata": {"warnings": ["missing_charset"]}});
    assert_eq!(
        settled_for(unrelated).await,
        "confirmed:C1:1700000000.000009"
    );
    // 잘림 경고 뒤에 자동으로 다시 보내지 않았다(게시 요청은 한 번)
    let rig = fresh().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(
            json!({"ok": true, "channel": "C1", "ts": "1700000000.000009",
            "response_metadata": {"warnings": ["message_truncated"]}}),
        ),
    );
    to_channel(&rig, 1, json!({"text": "안녕"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 1);
    assert_eq!(rig.deliverer.tick().await.unwrap(), 0);
    assert_eq!(posts(&rig).len(), 1);

    // 글자판으로 내린 재시도도 같은 확인을 거친다
    let rig = fresh().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_blocks"})),
    );
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": true, "channel": "C2", "ts": "1700000000.000009"})),
    );
    to_channel(&rig, 1, json!({"text": "안녕"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["held"]);

    // 첨부: 완료 응답이 올린 파일을 그대로(모든 항목이 비어 있지 않은 ID로, 개수·집합이 같게) 돌려주지 않으면 불명
    for reply in [
        json!({"ok": true}),
        json!({"ok": true, "files": []}),
        json!({"ok": true, "files": [{"id": "FOTHER"}]}),
        // 올린 파일 외의 항목이 더 있다·ID가 없는 항목이 끼었다·ID가 비었거나 문자열이 아니다
        json!({"ok": true, "files": [{"id": "FAAA1"}, {"id": "FOTHER"}]}),
        json!({"ok": true, "files": [{"id": "FAAA1"}, {"title": "no id"}]}),
        json!({"ok": true, "files": [{"id": "FAAA1"}, {"id": ""}]}),
        json!({"ok": true, "files": [{"id": ""}]}),
        json!({"ok": true, "files": [{"id": 7}]}),
        json!({"ok": true, "files": [{"id": "FAAA1"}, {"id": "FAAA1"}]}),
    ] {
        let rig = uploading(&["FAAA1"]).await;
        rig.slack.script(
            "files.completeUploadExternal",
            Scripted::Json(reply.clone()),
        );
        to_channel(&rig, 1, one_attachment());
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig), ["held"], "{reply}");
    }
    // 정확히 올린 파일들이면 확정한다
    let rig = uploading(&["FAAA1"]).await;
    rig.slack.script(
        "files.completeUploadExternal",
        Scripted::Json(json!({"ok": true, "files": [{"id": "FAAA1", "title": "a.txt"}]})),
    );
    to_channel(&rig, 1, one_attachment());
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["confirmed:C1:FAAA1"]);

    // 업로드 주소 응답의 주소·파일 ID가 없거나 문자열이 아니거나 비었으면 아무것도 올라가지 않았으니 모두 같은 `rejected`다(운영자 보류 `held`가 아니다): 업로드 0·완료 요청 0
    for slot in [
        json!({"ok": true, "upload_url": "@URL@"}),
        json!({"ok": true, "upload_url": "@URL@", "file_id": 7}),
        json!({"ok": true, "upload_url": "@URL@", "file_id": ""}),
        json!({"ok": true, "upload_url": "@URL@", "file_id": null}),
        json!({"ok": true, "file_id": "FAAA1"}),
        json!({"ok": true, "upload_url": 5, "file_id": "FAAA1"}),
        json!({"ok": true, "upload_url": "", "file_id": "FAAA1"}),
        json!({"ok": true}),
    ] {
        let rig = fresh().await;
        let url = format!("{}upload/0", rig.slack.base.trim_end_matches("api/"));
        let slot = serde_json::from_str::<Value>(&slot.to_string().replace("@URL@", &url)).unwrap();
        rig.slack
            .script("files.getUploadURLExternal", Scripted::Json(slot.clone()));
        to_channel(&rig, 1, one_attachment());
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig), ["rejected"], "{slot}");
        assert!(rig.slack.uploads().is_empty(), "{slot}");
        assert!(
            rig.slack.calls("files.completeUploadExternal").is_empty(),
            "{slot}"
        );
    }
    // 주소를 받는 요청 자체가 불명으로 끝나면(5xx) 그대로 불명이다(응답을 확인할 수 없다)
    let rig = fresh().await;
    rig.slack.script(
        "files.getUploadURLExternal",
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: String::new(),
        },
    );
    to_channel(&rig, 1, one_attachment());
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig), ["held"]);
    assert!(rig.slack.calls("files.completeUploadExternal").is_empty());

    // 업로드 주소를 받을 때 파일 ID가 비었거나 이미 받은 ID와 겹치면 아무것도 게시하지 않고 거절한다(완료 호출을 부르지 않는다)
    for ids in [vec![""], vec!["FAAA1", "FAAA1"]] {
        let rig = uploading(&ids).await;
        to_channel(&rig, 1, two_attachments());
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig), ["rejected"], "{ids:?}");
        assert!(
            rig.slack.calls("files.completeUploadExternal").is_empty(),
            "{ids:?}"
        );
    }
}

fn one_attachment() -> Value {
    json!({"text": "보고서", "attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]})
}

fn two_attachments() -> Value {
    json!({"text": "보고서", "attachments": [
        {"name": "a.txt", "type": "text/plain", "data": "aGk="},
        {"name": "b.txt", "type": "text/plain", "data": "aGk="}]})
}

/// 업로드 주소를 받을 때 Slack이 정해진 파일 ID들을 차례로 돌려주는 조립(번호가 달라져도 시험이 흔들리지 않게)
async fn uploading(ids: &[&str]) -> Rig {
    let rig = fresh().await;
    let base = rig.slack.base.trim_end_matches("api/").to_owned();
    for (n, id) in ids.iter().enumerate() {
        rig.slack.script(
            "files.getUploadURLExternal",
            Scripted::Json(
                json!({"ok": true, "upload_url": format!("{base}upload/{n}"), "file_id": id}),
            ),
        );
    }
    rig
}

#[tokio::test]
async fn the_fake_slack_answers_like_slack_so_ordinary_sends_are_confirmed() {
    // 가짜 서버가 보낸 채널과 올린 파일을 되돌려 주므로 정상 발송은 그대로 확인된다(위 시험이 어긋난 응답만 불명으로 만든다는 대조)
    let rig = fresh().await;
    to_channel(&rig, 1, json!({"text": "정상"}));
    rig.outbox.push(outgoing(
        "personal",
        2,
        "direct",
        "slack:home/U1",
        direct("D1"),
        &json!({"text": "DM"}).to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    let all = settled(&rig);
    assert!(all.iter().all(|s| s.starts_with("confirmed:")), "{all:?}");
}

/// 승인 카드 갱신·안내도 발송과 같은 토큰 확인 길을 지난다: 보관소의 토큰이 다른 팀의 것으로 바뀌면 그 토큰으로 `chat.update`·`chat.postEphemeral`을 부르지 않는다
/// (결정은 클릭한 사람의 것이라 기록은 그대로다).
#[tokio::test]
async fn a_card_refresh_goes_through_the_same_token_check_as_sending() {
    let secrets = default_secrets();
    let rig = support::rig_full(
        vec![support::personal()],
        secrets.clone(),
        support::host(""),
        None,
        Some(TTL),
        |_| {},
    )
    .await;
    assert!(rig.slack.connected().await);
    let card = json!({"nonce": "n1", "title": "일정 삭제", "lines": ["대상: 산책"]}).to_string();
    rig.ledger
        .put("n1", "personal", ApprovalState::Approved, &card);
    // 보관소의 토큰이 다른 팀의 것으로 바뀌고 기억이 끝난 뒤에 버튼을 누른다
    secrets.set("personal", "bot-token", b"other-team-token");
    rig.slack.set_auth("T9", "UBOT");
    wait_out_the_cache().await;
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "C1",
            "U1",
            "5.5",
            "bot_approve",
            "n1",
        ))
        .await;
    for _ in 0..100 {
        if !rig.broker.decisions().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rig.broker.decisions().len(), 1, "결정은 기록된다");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rig.slack.calls("chat.update").is_empty());
    assert!(rig.slack.calls("chat.postEphemeral").is_empty());
    assert!(
        rig.slack
            .calls("auth.test")
            .iter()
            .any(|call| call.token == "other-team-token"),
        "새 토큰은 확인했다"
    );
}

/// 알림·화면 읽기 프로그램이 읽는 최상위 `text`에는 본문 **전체**와 승인 카드의 필수 내용(제목·줄)이 들어간다 — 블록을 못 보는 사람도 무엇을 승인하는지 안다.
#[tokio::test]
async fn the_top_level_text_carries_the_whole_body_and_the_approval_it_asks_for() {
    let card =
        json!({"nonce": "n1", "title": "일정 삭제", "lines": ["대상: 산책", "되돌릴 수 없어요"]});
    // 3,000자를 넘는 본문과 카드를 함께 보낸다
    let rig = fresh().await;
    let body = "가".repeat(3_100);
    to_channel(&rig, 1, json!({"text": body, "approval": card}));
    rig.deliverer.tick().await.unwrap();
    let sent = posts(&rig);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].body["text"].as_str().unwrap(),
        format!("{body}\n\n일정 삭제\n대상: 산책\n되돌릴 수 없어요")
    );
    // 블록은 그대로다: 본문 블록 전체 + 카드 + 버튼
    assert_eq!(sent[0].body["blocks"][0]["text"].as_str().unwrap(), body);
    assert!(
        sent[0].body["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "actions")
    );

    // 알림 멘션은 최상위 글에서도 글자 그대로다
    let rig = fresh().await;
    to_channel(
        &rig,
        1,
        json!({"text": "<!channel> 확인해 주세요", "approval": card}),
    );
    rig.deliverer.tick().await.unwrap();
    let text = posts(&rig)[0].body["text"].as_str().unwrap().to_owned();
    assert!(
        text.starts_with("&lt;!channel> 확인해 주세요\n\n"),
        "{text}"
    );
    assert!(!text.contains("<!"), "{text}");

    // 카드만 있으면 카드의 내용이다
    let rig = fresh().await;
    to_channel(&rig, 1, json!({"approval": card}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(
        posts(&rig)[0].body["text"].as_str().unwrap(),
        "일정 삭제\n대상: 산책\n되돌릴 수 없어요"
    );

    // 담지 못하는 카드는 본문 뒤에 관리 화면 안내가 붙는다(승인 대상은 거기서 확인한다)
    let rig = fresh().await;
    let long = json!({"nonce": "n2", "title": "큰 변경", "lines": vec!["줄".repeat(500); 7]});
    to_channel(&rig, 1, json!({"text": body, "approval": long}));
    rig.deliverer.tick().await.unwrap();
    let text = posts(&rig)[0].body["text"].as_str().unwrap().to_owned();
    assert!(text.starts_with(&body), "본문 전체가 먼저 와요");
    assert!(
        text.contains("https://admin.example/approvals") && text.contains("승인이 필요한 요청"),
        "{}",
        text.chars().skip(3_100).collect::<String>()
    );
}

/// 시험 도우미(`Call::posted`)는 운영 발송기가 «올라갔다»로 확정할 응답만 게시로 본다: 정상 HTTP·`ok:true`·응답 채널이 요청 채널과 같음·유효한 `ts`·잘림 경고 없음.
#[tokio::test]
async fn the_test_helper_counts_a_post_only_when_the_sender_would_confirm_it() {
    use bot_gateway::slack::api::{Body, SlackApi};
    let slack = bot_gateway::testing::slack::FakeSlack::start().await;
    let api = SlackApi::new(Some(&slack.base));
    let ts = "1700000000.000009";
    let cases = [
        (
            Scripted::Json(json!({"ok": true, "channel": "D1", "ts": ts})),
            true,
        ),
        // 확정하지 않을 응답들
        (
            Scripted::Json(json!({"ok": true, "channel": "D2", "ts": ts})),
            false,
        ),
        (Scripted::Json(json!({"ok": true, "channel": "D1"})), false),
        (
            Scripted::Json(json!({"ok": true, "channel": "D1", "ts": "later"})),
            false,
        ),
        (
            Scripted::Json(json!({"ok": true, "channel": "D1", "ts": ts,
                "response_metadata": {"warnings": ["message_truncated"]}})),
            false,
        ),
        (
            Scripted::Json(json!({"ok": false, "error": "channel_not_found"})),
            false,
        ),
        (
            Scripted::Raw {
                status: 502,
                retry_after: None,
                body: json!({"ok": true, "channel": "D1", "ts": ts}).to_string(),
            },
            false,
        ),
    ];
    for (scripted, confirmed) in cases {
        slack.script("chat.postMessage", scripted);
        let message = json!({"channel": "D1", "text": "카드"});
        let _ = api
            .call("chat.postMessage", "token", Body::Json(&message))
            .await;
        let call = slack.calls("chat.postMessage").pop().unwrap();
        assert_eq!(
            call.posted(),
            confirmed.then(|| ("D1".to_owned(), ts.to_owned())),
            "{:?} {}",
            call.reply,
            call.status
        );
    }
    // 502에 ok:true를 돌려줘도 응답 본문은 기록되지만 성공으로 세지 않는다
    assert!(!slack.calls("chat.postMessage").last().unwrap().succeeded());
}

/// F2: 확인된 채널 게시(영수증)는 «봇이 이 스레드에 참여했다»는 증거로 남는다 — 스레드 안의 회신은 그 루트, 최상위 글은 글 자신. 확인되지 않은 게시·DM·첨부만 있는
/// 최상위 글은 남기지 않는다.
#[tokio::test]
async fn a_confirmed_channel_post_is_recorded_as_thread_evidence() {
    let rig = fresh().await;
    // 스레드 안의 회신: 루트는 그 스레드
    to_channel(&rig, 1, json!({"text": "회신"}));
    rig.deliverer.tick().await.unwrap();
    let notes = rig.threads.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    let n = &notes[0];
    assert_eq!(
        (
            n.profile.0.as_str(),
            n.binding.0.as_str(),
            n.team.as_str(),
            n.bot.as_str(),
            n.channel.as_str(),
            n.root_ts.as_str()
        ),
        ("personal", "team", "T1", "UBOT", "C1", THREAD)
    );

    // 스레드 없이 채널에 올린 최상위 글: 그 글의 ts가 루트다(그 아래로 사람 답글이 달린다)
    let mut top = outgoing(
        "personal",
        2,
        "team",
        "slack:home/U1",
        bot_core::state::Conversation {
            kind: bot_core::state::ConversationKind::Channel,
            id: "C1".into(),
            thread: None,
        },
        &json!({"text": "공지"}).to_string(),
    );
    top.call = bot_core::id::CallId("reply-2".into());
    rig.outbox.push(top);
    rig.deliverer.tick().await.unwrap();
    let roots: Vec<String> = rig.threads.notes().into_iter().map(|n| n.root_ts).collect();
    assert_eq!(roots.len(), 2);
    assert_eq!(roots[0], THREAD);
    assert_ne!(roots[1], THREAD, "{roots:?}");
    assert!(roots[1].contains('.'), "{roots:?}");

    // DM 회신·확인되지 않은 게시(Slack가 거절)는 남기지 않는다
    rig.outbox.push(outgoing(
        "personal",
        3,
        "direct",
        "slack:home/U1",
        direct("D1"),
        &json!({"text": "DM"}).to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "channel_not_found"})),
    );
    to_channel(&rig, 4, json!({"text": "거절될 글"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(rig.threads.notes().len(), 2);
}

/// 증거를 기록하지 못해도(저장소 장애) 이미 보낸 회신의 결과는 그대로 확정이다.
#[tokio::test]
async fn a_failed_evidence_write_does_not_change_the_delivery_outcome() {
    let rig = fresh().await;
    rig.threads.fail_writes(true);
    to_channel(&rig, 1, json!({"text": "회신"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig).len(), 1);
    assert!(
        settled(&rig)[0].starts_with("confirmed"),
        "{:?}",
        settled(&rig)
    );
    assert!(rig.threads.notes().is_empty());
}
