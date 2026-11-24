//! 봇 토큰을 바꾸는 동안(보관소에는 새 값, 이 프로세스의 기억에는 옛 값) 옛 토큰이 받는 Slack의 거절(`invalid_auth`·`token_revoked`·`token_expired`)은 영구 거절이 아니다.
//! 인증 단계에서 걸러져 아무것도 실행되지 않았으니 미실행(`NotSent`)으로 정산해 다시 시도하게 하고, 기억한 토큰을 버려 다음 시도가 자격을 다시 읽고 확인한 새 토큰으로 보내게 한다.
//! 같은 거절이 계속되면 거절된 토큰으로 게시 API를 다시 두드리지 않고(`auth.test`가 막는다) 재시도 한도는 저장소(`retries_exhausted`)가 센다.
mod delivery_support;
mod support;

use bot_core::{
    cards::{CardJob, CardOutcome},
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId},
    intake::NotSentReason,
    ledger::ApprovalState,
};
use bot_gateway::{
    host::HostConfig,
    reply::Card,
    slack::{
        api::{Body, Raw, SlackApi},
        creds::Credentials,
        send::CardPost,
    },
    testing::{
        MemorySecrets,
        cards::MemoryCards,
        ports::outgoing,
        slack::{FakeSlack, Scripted, button_frame},
    },
};
use delivery_support::*;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use support::default_secrets;

const CODES: [&str; 3] = ["invalid_auth", "token_revoked", "token_expired"];
/// 기억이 끝나지 않게 길게 둔다: 옛 토큰이 기억에 남아 있는 동안(실제로는 최대 30초)의 일을 만든다
const LONG: Duration = Duration::from_secs(3600);
const NOT_SENT: &str = "notsent:BeforeWrite:0";

/// 새 조립(지역 변수 `rig`가 함수 `rig`를 가리지 않게 따로 부른다)
async fn fresh() -> Rig {
    rig().await
}

/// 첫 회신을 보내 옛 토큰(`test-bot-1`)이 확인돼 기억에 남은 조립
async fn primed() -> (Rig, Arc<MemorySecrets>) {
    let secrets = default_secrets();
    let rig = rig_ttl(secrets.clone(), LONG).await;
    to_channel(&rig, 1, json!({"text": "첫째"}));
    rig.deliverer.tick().await.unwrap();
    assert!(settled(&rig)[0].starts_with("confirmed:"));
    (rig, secrets)
}

/// 저장소가 미실행 정산을 받아 백오프 뒤 새 세대로 같은 회신을 다시 내놓는 것을 흉내 낸다
fn offer_again(rig: &Rig, seq: u32, generation: u64, body: Value) {
    let mut item = outgoing(
        "personal",
        seq,
        "team",
        "slack:home/U1",
        channel("C1"),
        &body.to_string(),
    );
    item.key.generation = Generation(generation);
    rig.outbox.push(item);
}

fn tokens(rig: &Rig) -> Vec<String> {
    posts(rig).into_iter().map(|call| call.token).collect()
}

fn auth_checks(rig: &Rig) -> Vec<String> {
    rig.slack
        .calls("auth.test")
        .into_iter()
        .map(|call| call.token)
        .collect()
}

#[tokio::test]
async fn a_reply_sent_while_the_bot_token_is_being_replaced_is_sent_once_with_the_new_token() {
    let (rig, secrets) = primed().await;
    // 토큰 교체: 보관소에는 새 값이 들어갔고 Slack는 옛 토큰을 거절한다. 이 프로세스의 기억에는 아직 옛 토큰이 있다
    secrets.set("personal", "bot-token", b"test-bot-2");
    rig.slack.revoke("test-bot-1", "invalid_auth");
    to_channel(&rig, 2, json!({"text": "둘째"}));
    rig.deliverer.tick().await.unwrap();
    // 옛 토큰으로 한 번 부딪혔다: 영구 거절이 아니라 아무것도 보내지 않은 미실행이다
    assert_eq!(settled(&rig)[1], NOT_SENT);
    // 저장소가 백오프 뒤 새 세대로 다시 내놓는다: 자격을 다시 읽어 확인한 새 토큰으로 한 번 성공한다
    offer_again(&rig, 2, 2, json!({"text": "둘째"}));
    rig.deliverer.tick().await.unwrap();
    let all = settled(&rig);
    assert_eq!(all.len(), 3, "{all:?}");
    assert!(all[2].starts_with("confirmed:C1:"), "{all:?}");
    assert!(!all.iter().any(|outcome| outcome == "rejected"), "{all:?}");
    // 첫째(옛 토큰)·둘째의 첫 시도(옛 토큰, 거절)·둘째의 다시 시도(새 토큰)
    assert_eq!(tokens(&rig), ["test-bot-1", "test-bot-1", "test-bot-2"]);
    let said: Vec<String> = posts(&rig)
        .iter()
        .filter(|call| call.succeeded())
        .map(|call| call.body["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(said, ["첫째", "둘째"], "유실도 중복도 없어요");
    // 새 토큰은 쓰기 전에 등록한 팀의 것인지 확인했다
    assert_eq!(auth_checks(&rig), ["test-bot-1", "test-bot-2"]);
}

#[tokio::test]
async fn a_token_that_stays_rejected_is_never_final_and_is_not_posted_with_again() {
    for code in CODES {
        let (rig, _secrets) = primed().await;
        // 보관소는 그대로인데 Slack가 토큰을 거절한다(철회·만료)
        rig.slack.revoke("test-bot-1", code);
        to_channel(&rig, 2, json!({"text": "둘째"}));
        rig.deliverer.tick().await.unwrap();
        assert_eq!(settled(&rig)[1], NOT_SENT, "{code}");
        // 다시 시도해도 같은 토큰이다: 자격을 다시 읽어 확인(`auth.test`)하는 데서 막히고 거절된 토큰으로 게시 API를 두드리지 않는다. 끝내 거절로 정산하지도 않는다
        for generation in 2..=3 {
            offer_again(&rig, 2, generation, json!({"text": "둘째"}));
            rig.deliverer.tick().await.unwrap();
        }
        let all = settled(&rig);
        assert_eq!(all[1..], [NOT_SENT, NOT_SENT, NOT_SENT], "{code}: {all:?}");
        assert_eq!(
            posts(&rig).len(),
            2,
            "{code}: 첫째(성공)와 첫 시도(거절)뿐이에요"
        );
        assert_eq!(
            auth_checks(&rig).len(),
            3,
            "{code}: 처음 + 다시 시도마다 한 번"
        );
    }
}

fn one_attachment() -> Value {
    json!({"text": "보고서", "attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]})
}

#[tokio::test]
async fn an_upload_treats_a_rejected_token_as_not_sent_at_every_step() {
    // 주소를 받는 단계에서
    let (rig, _secrets) = primed().await;
    rig.slack.revoke("test-bot-1", "token_revoked");
    rig.outbox.push(outgoing(
        "personal",
        2,
        "team",
        "slack:home/U1",
        channel("C1"),
        &one_attachment().to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[1], NOT_SENT);
    assert_eq!(rig.slack.calls("files.getUploadURLExternal").len(), 1);
    assert!(rig.slack.calls("files.completeUploadExternal").is_empty());
    assert!(rig.slack.uploads().is_empty());
    // 거절된 토큰은 기억에서 버려졌다: 다시 시도하면 자격을 다시 확인하는 데서 막혀 주소 요청도 다시 나가지 않는다
    offer_again(&rig, 2, 2, one_attachment());
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[2], NOT_SENT);
    assert_eq!(rig.slack.calls("files.getUploadURLExternal").len(), 1);
    assert_eq!(auth_checks(&rig).len(), 2);

    // 게시하는 완료 호출에서(앞 단계는 통과했다): 아무것도 게시되지 않았다
    let (rig, _secrets) = primed().await;
    rig.slack.script(
        "files.completeUploadExternal",
        Scripted::Json(json!({"ok": false, "error": "token_expired"})),
    );
    rig.outbox.push(outgoing(
        "personal",
        2,
        "team",
        "slack:home/U1",
        channel("C1"),
        &one_attachment().to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[1], NOT_SENT);
    assert_eq!(rig.slack.calls("files.getUploadURLExternal").len(), 1);
    assert_eq!(rig.slack.calls("files.completeUploadExternal").len(), 1);
    // 다음 시도는 자격을 다시 확인한다(기억에서 버렸다)
    assert_eq!(auth_checks(&rig).len(), 1);
    offer_again(&rig, 2, 2, one_attachment());
    rig.deliverer.tick().await.unwrap();
    assert_eq!(auth_checks(&rig).len(), 2);
    assert!(
        settled(&rig)[2].starts_with("confirmed:"),
        "{:?}",
        settled(&rig)
    );
}

/// 블록 한도를 넘는 본문은 글자판 메시지 하나로 간다: 이 길도 같은 분류·같은 기억 정리를 따른다
#[tokio::test]
async fn the_plain_text_path_forgets_a_rejected_token_too() {
    let (rig, _secrets) = primed().await;
    rig.slack.revoke("test-bot-1", "invalid_auth");
    let long = "가".repeat(12_000);
    to_channel(&rig, 2, json!({"text": long}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[1], NOT_SENT);
    let sent = posts(&rig);
    assert_eq!(sent.len(), 2);
    assert!(sent[1].body.get("blocks").is_none(), "글자판 메시지예요");
    offer_again(&rig, 2, 2, json!({"text": long}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(settled(&rig)[2], NOT_SENT);
    assert_eq!(auth_checks(&rig).len(), 2, "다시 확인했어요");
    assert_eq!(posts(&rig).len(), 2, "거절된 토큰으로 다시 게시하지 않아요");
}

#[tokio::test]
async fn only_the_token_slack_refused_is_forgotten() {
    let slack = FakeSlack::start().await;
    let host = Arc::new(HostConfig::from_json(HOST).unwrap());
    let creds =
        Credentials::new(default_secrets(), host, SlackApi::new(Some(&slack.base))).with_ttl(LONG);
    let profile = ProfileId("personal".into());
    let body = json!({"channel": "C1", "text": "x"});
    let current = || async {
        creds
            .bot_token(&profile, "home")
            .await
            .map(|t| t.to_string())
    };
    assert_eq!(current().await.as_deref(), Some("test-bot-1"));
    assert_eq!(slack.calls("auth.test").len(), 1);

    // 다른 토큰의 거절은 지금 기억을 건드리지 않는다
    creds.reject("some-older-token");
    assert_eq!(current().await.as_deref(), Some("test-bot-1"));
    assert_eq!(slack.calls("auth.test").len(), 1);

    // 토큰 탓이 아닌 응답(채널 거절·속도 제한·불명)은 기억을 건드리지 않는다
    for script in [
        Scripted::Json(json!({"ok": false, "error": "channel_not_found"})),
        Scripted::Raw {
            status: 429,
            retry_after: Some(1),
            body: String::new(),
        },
        Scripted::Raw {
            status: 500,
            retry_after: None,
            body: String::new(),
        },
    ] {
        slack.script("chat.postMessage", script);
        creds
            .call("chat.postMessage", "test-bot-1", Body::Json(&body))
            .await;
    }
    assert_eq!(current().await.as_deref(), Some("test-bot-1"));
    assert_eq!(slack.calls("auth.test").len(), 1);

    // 이 토큰을 Slack가 거절하면 버린다: 다음에 다시 읽고 확인하는데 거절된 토큰은 확인을 통과하지 못한다
    slack.revoke("test-bot-1", "invalid_auth");
    let raw = creds
        .call("chat.postMessage", "test-bot-1", Body::Json(&body))
        .await;
    assert_eq!(raw, Raw::TokenRejected);
    assert_eq!(current().await, None);
    assert_eq!(slack.calls("auth.test").len(), 2);
}

fn card() -> Card {
    Card {
        nonce: "n-1".into(),
        title: "예약 목적지 승인".into(),
        lines: vec!["수신자: 팀 채널".into()],
    }
}

fn not_sent() -> CardPost {
    CardPost::NotSent {
        reason: NotSentReason::BeforeWrite,
        retry_after: None,
    }
}

#[tokio::test]
async fn a_rejected_token_never_makes_the_dm_card_unavailable() {
    let owner = PrincipalId("slack:home/U1".into());
    let profile = ProfileId("personal".into());
    // 열기에서: 아무것도 올라가지 않았다
    let rig = fresh().await;
    rig.slack.script(
        "conversations.open",
        Scripted::Json(json!({"ok": false, "error": "token_revoked"})),
    );
    let sender = rig.sender();
    let got = sender
        .post_owner_card(&profile, "home", &owner, &card())
        .await;
    assert_eq!(got, not_sent());
    assert!(posts(&rig).is_empty());

    // 게시에서(열기는 통과했다): 인증 단계에서 걸러져 아무것도 올라가지 않았다
    let rig = fresh().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Json(json!({"ok": false, "error": "invalid_auth"})),
    );
    let sender = rig.sender();
    let got = sender
        .post_owner_card(&profile, "home", &owner, &card())
        .await;
    assert_eq!(got, not_sent());
    assert_eq!(rig.slack.calls("conversations.open").len(), 1);
    assert_eq!(posts(&rig).len(), 1);
    // 다음 시도는 자격을 다시 확인하고(기억에서 버렸다) 한 번 올린다
    assert_eq!(auth_checks(&rig).len(), 1);
    let got = sender
        .post_owner_card(&profile, "home", &owner, &card())
        .await;
    assert!(matches!(got, CardPost::Posted { .. }), "{got:?}");
    assert_eq!(auth_checks(&rig).len(), 2);
}

fn job() -> CardJob {
    CardJob {
        nonce: ApprovalId("n-1".into()),
        profile: ProfileId("personal".into()),
        binding: BindingId("team".into()),
        owner: PrincipalId("slack:home/U1".into()),
        card: JsonPayload(
            json!({"nonce": "n-1", "title": "예약 목적지 승인", "lines": ["수신자: 팀 채널"]})
                .to_string(),
        ),
        generation: Generation(1),
    }
}

#[tokio::test]
async fn a_dm_card_requested_while_the_bot_token_is_being_replaced_is_posted_once_after_it() {
    let (rig, secrets) = primed().await;
    secrets.set("personal", "bot-token", b"test-bot-2");
    rig.slack.revoke("test-bot-1", "token_expired");
    let cards = MemoryCards::new();
    cards.push(job());
    let deliverer = rig.card_deliverer(cards.clone());
    // 첫 시도: 기억에 남은 옛 토큰이 거절된다 — 카드를 닫지 않고(`Unavailable` 아님) 다시 잡히게 돌려보낸다
    deliverer.tick().await.unwrap();
    assert_eq!(
        cards.settled(),
        [(
            "n-1".to_owned(),
            CardOutcome::NotSent {
                reason: NotSentReason::BeforeWrite,
                retry_after: None
            }
        )]
    );
    assert_eq!(cards.waiting(), 1);
    assert_eq!(
        posts(&rig).len(),
        1,
        "첫째 회신뿐이에요: 카드는 올라가지 않았어요"
    );
    // 다시 잡힌 두 번째 시도: 새 토큰으로 한 번 올라간다
    deliverer.tick().await.unwrap();
    let settled = cards.settled();
    assert_eq!(settled.len(), 2, "{settled:?}");
    assert!(
        matches!(settled[1].1, CardOutcome::Posted { .. }),
        "{settled:?}"
    );
    let cards_in_dm: Vec<_> = posts(&rig)
        .into_iter()
        .filter(|call| call.body["channel"] == "D1")
        .collect();
    assert_eq!(cards_in_dm.len(), 1);
    assert_eq!(cards_in_dm[0].token, "test-bot-2");
    assert!(cards_in_dm[0].posted().is_some());
}

/// 카드 갱신·안내도 같은 길을 지난다: 거절당한 토큰은 거기서도 버려져 다음 사용이 자격을 다시 확인한다.
#[tokio::test]
async fn a_card_refresh_the_token_was_refused_for_makes_the_next_use_check_the_token_again() {
    let rig = support::rig_full(
        vec![support::personal()],
        default_secrets(),
        support::host(""),
        None,
        Some(LONG),
        |slack| {
            slack.script(
                "chat.update",
                Scripted::Json(json!({"ok": false, "error": "invalid_auth"})),
            );
        },
    )
    .await;
    assert!(rig.slack.connected().await);
    let checked = rig.slack.calls("auth.test").len();
    let card = json!({"nonce": "n1", "title": "일정 삭제", "lines": ["대상: 산책"]}).to_string();
    rig.ledger
        .put("n1", "personal", ApprovalState::Approved, &card);
    for envelope in ["B1", "B2"] {
        rig.slack
            .push(button_frame(
                envelope,
                "T1",
                "C1",
                "U1",
                "5.5",
                "bot_approve",
                "n1",
            ))
            .await;
        // 클릭 하나가 끝나야(ack) 다음 클릭을 보낸다
        assert!(
            rig.slack
                .until(|slack| slack.acks().contains(&envelope.to_owned()))
                .await,
            "{envelope}"
        );
    }
    let updates = rig.slack.calls("chat.update");
    assert_eq!(updates.len(), 2);
    assert!(!updates[0].succeeded() && updates[1].succeeded());
    assert_eq!(
        rig.slack.calls("auth.test").len(),
        checked + 1,
        "첫 거절 뒤 두 번째 갱신 전에 토큰을 다시 확인했어요"
    );
}

/// 누른 사람에게만 보이는 안내(`chat.postEphemeral`)도 같은 길이다.
#[tokio::test]
async fn a_private_notice_the_token_was_refused_for_also_makes_the_next_use_check_the_token_again()
{
    let rig = support::rig_full(
        vec![support::personal()],
        default_secrets(),
        support::host(""),
        None,
        Some(LONG),
        |slack| {
            slack.script(
                "chat.postEphemeral",
                Scripted::Json(json!({"ok": false, "error": "token_revoked"})),
            );
        },
    )
    .await;
    assert!(rig.slack.connected().await);
    let checked = rig.slack.calls("auth.test").len();
    // 등록되지 않은 사람(U9)의 클릭에는 «등록된 사용자만» 안내가 간다
    for envelope in ["B1", "B2"] {
        rig.slack
            .push(button_frame(
                envelope,
                "T1",
                "C1",
                "U9",
                "5.5",
                "bot_approve",
                "n1",
            ))
            .await;
        assert!(
            rig.slack
                .until(|slack| slack.acks().contains(&envelope.to_owned()))
                .await,
            "{envelope}"
        );
    }
    let notices = rig.slack.calls("chat.postEphemeral");
    assert_eq!(notices.len(), 2);
    assert!(!notices[0].succeeded() && notices[1].succeeded());
    assert_eq!(
        rig.slack.calls("auth.test").len(),
        checked + 1,
        "첫 거절 뒤 두 번째 안내 전에 토큰을 다시 확인했어요"
    );
}
