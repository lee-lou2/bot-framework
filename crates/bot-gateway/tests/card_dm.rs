//! 소유자 DM으로 승인 카드를 올리는 동작(가짜 Slack): 같은 설치에서 소유자 한 명으로 `conversations.open` → 확인한 `D…` 채널에 온전한 카드를 한 번 올리고 응답이 말한 채널·ts를 확인한다.
//! 성공·오류·응답 유실·다른 사용자/설치·크기 초과·재전달(자동 재게시 없음)을 가른다.
mod delivery_support;
mod support;

use bot_core::{
    id::{PrincipalId, ProfileId},
    intake::NotSentReason,
};
use bot_gateway::{
    reply::Card,
    slack::send::{CardPost, slack_user},
    testing::slack::Scripted,
};
use delivery_support::*;
use serde_json::{Value, json};
use std::time::Duration;

/// 새 조립(지역 변수 `rig`가 함수 `rig`를 가리지 않게 따로 부른다)
async fn fresh() -> Rig {
    rig().await
}

fn owner(text: &str) -> PrincipalId {
    PrincipalId(text.to_owned())
}

fn card() -> Card {
    Card {
        nonce: "n-1".into(),
        title: "예약 목적지 승인".into(),
        lines: vec![
            "수신자: 팀 채널".into(),
            "지시문: 자료 도착을 알려 줘".into(),
        ],
    }
}

fn personal() -> ProfileId {
    ProfileId("personal".into())
}

/// 승인 카드를 소유자 DM에 올려 본다(기본 가짜 Slack)
async fn post(rig: &Rig, who: &PrincipalId, card: &Card) -> CardPost {
    let sender = rig.sender();
    sender.post_owner_card(&personal(), "home", who, card).await
}

fn open_calls(rig: &Rig) -> Vec<bot_gateway::testing::slack::Call> {
    rig.slack.calls("conversations.open")
}

fn card_posts(rig: &Rig) -> Vec<bot_gateway::testing::slack::Call> {
    rig.slack.calls("chat.postMessage")
}

#[tokio::test]
async fn the_whole_card_goes_to_the_owners_dm_once_and_the_receipt_is_what_slack_answered() {
    let rig = fresh().await;
    let got = post(&rig, &owner("slack:home/U1"), &card()).await;
    let opened = open_calls(&rig);
    let posted = card_posts(&rig);
    // 소유자 한 명으로 열고(`users=U1`) 확인한 DM(`D1`)에 한 번 올린다 — 채널·스레드에는 아무것도 가지 않는다
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].body["users"], "U1");
    assert_eq!(opened[0].token, "test-bot-1");
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].body["channel"], "D1");
    assert!(posted[0].body.get("thread_ts").is_none());
    // 카드는 잘리지 않았다: 제목·모든 줄·승인/거절 버튼(값은 nonce)
    let blocks = posted[0].body["blocks"].as_array().unwrap();
    assert_eq!(blocks[0]["text"]["text"], "예약 목적지 승인");
    assert_eq!(
        blocks[1]["text"]["text"],
        "수신자: 팀 채널\n지시문: 자료 도착을 알려 줘"
    );
    let buttons = blocks.last().unwrap()["elements"].as_array().unwrap();
    assert!(buttons.iter().all(|b| b["value"] == "n-1"));
    assert_eq!(
        posted[0].body["text"],
        "예약 목적지 승인\n수신자: 팀 채널\n지시문: 자료 도착을 알려 줘"
    );
    // 영수증은 응답이 말한 채널과 ts다
    let answered = posted[0].posted().unwrap();
    assert_eq!(
        got,
        CardPost::Posted {
            team: "T1".into(),
            dm: answered.0,
            ts: answered.1
        }
    );
    assert!(matches!(&got, CardPost::Posted { dm, .. } if dm == "D1"));
}

#[tokio::test]
async fn an_owner_who_is_not_a_user_of_this_installation_gets_no_dm_and_no_slack_call() {
    let rig = fresh().await;
    for who in [
        "slack:other/U1", // 다른 설치의 사용자
        "local:owner",    // Slack 사용자가 아니다
        "http:inbox/U1",
        "slack:home/u1", // 모양이 틀린 ID
        "slack:home/",
        "slack:home/U1/extra",
        "slack:home",
        "U1",
        "",
    ] {
        assert_eq!(
            post(&rig, &owner(who), &card()).await,
            CardPost::Unavailable,
            "{who:?}"
        );
    }
    assert!(
        rig.slack.all_calls().is_empty(),
        "DM 주소를 정할 수 없으면 Slack을 부르지 않아요"
    );
    assert_eq!(slack_user(&owner("slack:home/U1")), Some(("home", "U1")));
}

#[tokio::test]
async fn a_card_that_does_not_fit_is_never_replaced_by_a_notice_or_a_cut_card() {
    let rig = fresh().await;
    let long = Card {
        nonce: "n-2".into(),
        title: "큰 변경".into(),
        lines: vec!["줄".repeat(500); 7],
    };
    assert_eq!(
        post(&rig, &owner("slack:home/U1"), &long).await,
        CardPost::Unavailable
    );
    assert!(
        rig.slack.all_calls().is_empty(),
        "열기도 올리기도 하지 않았어요"
    );
}

#[tokio::test]
async fn a_dm_that_cannot_be_opened_is_unavailable_and_nothing_is_posted() {
    // Slack이 이유를 밝힌 열기 거절은 이 소유자에게 보낼 DM 경로가 없다는 확정이다
    for code in [
        "user_not_found",
        "user_not_visible",
        "cannot_dm_bot",
        "missing_scope",
        "not_allowed_token_type",
        "users_list_not_supplied",
        "channel_not_found",
    ] {
        let rig = fresh().await;
        rig.slack.script(
            "conversations.open",
            Scripted::Json(json!({"ok": false, "error": code})),
        );
        let got = post(&rig, &owner("slack:home/U1"), &card()).await;
        assert_eq!(got, CardPost::Unavailable, "{code}");
        assert!(card_posts(&rig).is_empty(), "{code}");
    }
    // 열기는 ok인데 1:1 DM 채널이 아니다(없음·모양이 틀림·공개 채널 ID): 카드를 다른 곳에 올리지 않는다
    for reply in [
        json!({"ok": true}),
        json!({"ok": true, "channel": {}}),
        json!({"ok": true, "channel": {"id": "C1"}}),
        json!({"ok": true, "channel": {"id": "G1"}}),
        json!({"ok": true, "channel": {"id": 7}}),
        json!({"ok": true, "channel": {"id": "d1"}}),
        json!({"ok": true, "channel": "D1"}),
    ] {
        let rig = fresh().await;
        rig.slack
            .script("conversations.open", Scripted::Json(reply.clone()));
        assert_eq!(
            post(&rig, &owner("slack:home/U1"), &card()).await,
            CardPost::Unavailable,
            "{reply}"
        );
        assert!(card_posts(&rig).is_empty(), "{reply}");
    }
}

#[tokio::test]
async fn a_transient_open_failure_posts_nothing_and_asks_to_try_again() {
    let transient = |reason, retry_after| CardPost::NotSent {
        reason,
        retry_after,
    };
    // 속도 제한: 수신처가 준 대기 시간이 재시도 하한이다
    let rig = fresh().await;
    rig.slack.script(
        "conversations.open",
        Scripted::Raw {
            status: 429,
            retry_after: Some(7),
            body: String::new(),
        },
    );
    assert_eq!(
        post(&rig, &owner("slack:home/U1"), &card()).await,
        transient(NotSentReason::RateLimited, Some(Duration::from_secs(7)))
    );
    assert!(card_posts(&rig).is_empty());
    // 서버 사정(5xx·`internal_error`·응답 없음)은 열기가 멱등이라 아무것도 올라가지 않았다
    for scripted in [
        Scripted::Json(json!({"ok": false, "error": "internal_error"})),
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: String::new(),
        },
        Scripted::Raw {
            status: 200,
            retry_after: None,
            body: "not json".into(),
        },
    ] {
        let rig = fresh().await;
        rig.slack.script("conversations.open", scripted);
        assert_eq!(
            post(&rig, &owner("slack:home/U1"), &card()).await,
            transient(NotSentReason::BeforeWrite, None)
        );
        assert!(card_posts(&rig).is_empty());
    }
    // 같은 소유자로 다시 하면(기록된 재시도) 열기부터 다시 해 한 번만 올라간다
    let rig = fresh().await;
    rig.slack.script(
        "conversations.open",
        Scripted::Json(json!({"ok": false, "error": "internal_error"})),
    );
    assert!(matches!(
        post(&rig, &owner("slack:home/U1"), &card()).await,
        CardPost::NotSent { .. }
    ));
    assert!(matches!(
        post(&rig, &owner("slack:home/U1"), &card()).await,
        CardPost::Posted { .. }
    ));
    assert_eq!(open_calls(&rig).len(), 2);
    assert_eq!(card_posts(&rig).len(), 1);
}

#[tokio::test]
async fn a_credential_that_cannot_be_read_or_checked_sends_nothing() {
    // 자격 이름이 없다
    let rig = rig_with(
        support::personal(),
        support::secrets(&[("personal", "app-token", "x")]),
        None,
    )
    .await;
    assert_eq!(
        post(&rig, &owner("slack:home/U1"), &card()).await,
        CardPost::NotSent {
            reason: NotSentReason::BeforeWrite,
            retry_after: None
        }
    );
    assert!(rig.slack.all_calls().is_empty());
    // 토큰이 다른 팀의 것이다(등록한 팀이 아니면 DM을 열지 않는다)
    let rig = fresh().await;
    rig.slack.set_auth("T9", "UBOT");
    assert!(matches!(
        post(&rig, &owner("slack:home/U1"), &card()).await,
        CardPost::NotSent { .. }
    ));
    assert!(open_calls(&rig).is_empty() && card_posts(&rig).is_empty());
}

/// 카드를 올리는 요청의 응답을 이렇게 꾸미면
async fn posted_with(reply: Scripted) -> (CardPost, usize) {
    let rig = fresh().await;
    rig.slack.script("chat.postMessage", reply);
    let got = post(&rig, &owner("slack:home/U1"), &card()).await;
    (got, card_posts(&rig).len())
}

#[tokio::test]
async fn a_post_that_slack_refused_for_a_reason_is_unavailable_and_an_unclear_one_is_held_without_reposting()
 {
    // 이유를 밝힌 거절: 아무것도 올라가지 않았다 — 카드 한 번 시도, 글자판으로 내려 다시 올리지 않는다
    for code in [
        "channel_not_found",
        "is_archived",
        "msg_too_long",
        "invalid_blocks",
        "invalid_blocks_format",
        "msg_blocks_too_long",
        "not_in_channel",
    ] {
        let (got, posts) = posted_with(Scripted::Json(json!({"ok": false, "error": code}))).await;
        assert_eq!((got, posts), (CardPost::Unavailable, 1), "{code}");
    }
    // 속도 제한: 올라가지 않았다
    let (got, posts) = posted_with(Scripted::Raw {
        status: 429,
        retry_after: Some(3),
        body: String::new(),
    })
    .await;
    assert_eq!(
        (got, posts),
        (
            CardPost::NotSent {
                reason: NotSentReason::RateLimited,
                retry_after: Some(Duration::from_secs(3))
            },
            1
        )
    );
    // 올라갔는지 알 수 없다: 시간 초과·5xx·서버 오류 코드·ok인데 확인할 수 없는 응답(다른 채널·ts 없음·모양이 아닌 ts·잘림 경고)
    let unclear = [
        Scripted::Json(json!({"ok": false, "error": "internal_error"})),
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: String::new(),
        },
        Scripted::Raw {
            status: 200,
            retry_after: None,
            body: "not json".into(),
        },
        Scripted::Json(json!({"ok": true, "channel": "D2", "ts": "1700000000.000009"})),
        Scripted::Json(json!({"ok": true, "channel": "C1", "ts": "1700000000.000009"})),
        Scripted::Json(json!({"ok": true, "channel": "D1"})),
        Scripted::Json(json!({"ok": true, "ts": "1700000000.000009"})),
        Scripted::Json(json!({"ok": true, "channel": "D1", "ts": "later"})),
        Scripted::Json(
            json!({"ok": true, "channel": "D1", "ts": "1700000000.000009",
            "response_metadata": {"warnings": ["message_truncated"]}}),
        ),
    ];
    for reply in unclear {
        let shown: Value = match &reply {
            Scripted::Json(value) => value.clone(),
            Scripted::Raw { status, body, .. } => json!({"http": status, "body": body}),
            Scripted::Delayed { body, .. } => body.clone(),
        };
        let (got, posts) = posted_with(reply).await;
        assert_eq!((got, posts), (CardPost::Held, 1), "{shown}");
    }
}

/// 한 명에게만: 소유자마다 자기 DM이고(`U2`는 `D2`), 같은 소유자에게 다시 부르면 새 요청이다(자동 재전송이 없다는 것은 부르는 쪽이 `begin`으로 지킨다)
#[tokio::test]
async fn each_owner_gets_only_their_own_dm_and_the_card_never_goes_to_the_channel_it_came_from() {
    let rig = fresh().await;
    let first = post(&rig, &owner("slack:home/U1"), &card()).await;
    let second = post(&rig, &owner("slack:home/U2"), &card()).await;
    assert!(matches!(&first, CardPost::Posted { dm, .. } if dm == "D1"));
    assert!(matches!(&second, CardPost::Posted { dm, .. } if dm == "D2"));
    let users: Vec<_> = open_calls(&rig)
        .iter()
        .map(|call| call.body["users"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(users, ["U1", "U2"]);
    let channels: Vec<_> = card_posts(&rig)
        .iter()
        .map(|call| call.body["channel"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(channels, ["D1", "D2"], "채널(`C…`)에는 올리지 않아요");
}
