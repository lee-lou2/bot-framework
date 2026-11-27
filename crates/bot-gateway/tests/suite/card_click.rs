//! 소유자 DM 승인 카드의 클릭: 카드를 올리면(`CardDeliverer`) 영수증이 생기고, 그 영수증이 확정한 DM 메시지의 버튼만 소유자가 결정한다.
//! 원래 채널 바인딩만 있고 일반 DM 입력은 없는 프로필에서도 이 카드의 버튼은 동작하고(일반 DM 입구는 열리지 않는다),
//! 다른 곳·다른 사람·다른 팀의 클릭과 영수증 확정 전의 클릭은 결정을 소비하지 않는다.

use crate::support::*;
use bot_core::{
    cards::{CardJob, CardKey},
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId},
    ledger::ApprovalState,
    port::Error,
    profile::Adapter,
};
use bot_gateway::{
    slack::cards::CardDeliverer,
    testing::{cards::MemoryCards, profile, slack::button_frame},
};
use serde_json::json;

const NONCE: &str = "n1";

/// 원래 채널 바인딩만 있다(일반 DM 입력 바인딩이 없다). 소유자 U1, 다른 구성원 U2
fn channel_only() -> bot_gateway::testing::Fixture {
    profile("personal")
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
        .member("slack:home/U2", &["owner"])
        .role("owner", &["run"])
        .policy_scopes(&["run"])
}

fn card_json() -> String {
    json!({"nonce": NONCE, "title": "예약 목적지 승인", "lines": ["수신자: 팀 채널"]}).to_string()
}

fn job() -> CardJob {
    CardJob {
        key: CardKey {
            nonce: ApprovalId(NONCE.into()),
            attempt: 0,
        },
        profile: ProfileId("personal".into()),
        binding: BindingId("team".into()),
        owner: PrincipalId("slack:home/U1".into()),
        card: JsonPayload(card_json()),
        generation: Generation(1),
    }
}

/// 카드를 소유자 DM에 올려 영수증까지 만든 조립(저장소처럼 `Posted` 정산이 승인의 `delivery`에 영수증을 남긴다). 올라간 DM 채널과 메시지 `ts`를 함께 돌려준다.
async fn delivered(profiles: Vec<bot_gateway::testing::Fixture>) -> (Rig, String, String) {
    let rig = connected(profiles).await;
    let cards = MemoryCards::linked(rig.ledger.clone());
    rig.ledger
        .put(NONCE, "personal", ApprovalState::Pending, &card_json());
    cards.push(job());
    let deliverer = CardDeliverer::new(cards, rig.ingress.clone(), rig.gateway.sender());
    assert_eq!(deliverer.tick().await.unwrap(), 1);
    let (dm, ts) = rig.slack.calls("chat.postMessage")[0]
        .posted()
        .expect("카드가 올라갔어요");
    (rig, dm, ts)
}

fn ephemerals(rig: &Rig) -> Vec<String> {
    rig.slack
        .calls("chat.postEphemeral")
        .iter()
        .map(|c| c.body["text"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_owner_decides_on_the_dm_card_even_when_plain_dm_input_is_closed() {
    let (rig, dm, ts) = delivered(vec![channel_only()]).await;
    assert_eq!(dm, "D1");
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            &dm,
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(
        rig.broker.decisions(),
        [(NONCE.to_string(), "slack:home/U1".to_string(), true)]
    );
    // 카드는 그 DM 메시지가 갱신된다
    rig.ledger.set_state(NONCE, ApprovalState::Approved);
    rig.slack
        .push(button_frame("B2", "T1", &dm, "U1", &ts, "bot_deny", NONCE))
        .await;
    assert!(rig.slack.until(|s| s.acks() == ["B1", "B2"]).await);
    let update = &rig.slack.calls("chat.update")[0];
    assert_eq!(
        (update.body["channel"].as_str(), update.body["ts"].as_str()),
        (Some(dm.as_str()), Some(ts.as_str()))
    );
    assert!(ephemerals(&rig).is_empty());
}

#[tokio::test]
async fn a_click_anywhere_but_the_confirmed_dm_message_decides_nothing() {
    let (rig, dm, ts) = delivered(vec![channel_only()]).await;
    // 메시지를 복사한 곳(다른 채널·다른 DM)·다른 메시지·다른 구성원(U2)·메시지 없는 클릭
    for (id, channel, user, message_ts) in [
        ("B1", "C1", "U1", ts.as_str()),
        ("B2", "D2", "U1", ts.as_str()),
        ("B3", dm.as_str(), "U1", "1700000000.999999"),
        ("B4", dm.as_str(), "U2", ts.as_str()),
        ("B5", dm.as_str(), "U1", ""),
    ] {
        rig.slack
            .push(button_frame(
                id,
                "T1",
                channel,
                user,
                message_ts,
                "bot_approve",
                NONCE,
            ))
            .await;
    }
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 5)
            .await
    );
    // 영구적으로 처리할 수 없는 클릭이라 모두 ack한다(재전달로 되돌리지 않는다)
    assert!(
        rig.slack
            .until(|s| ["B1", "B2", "B3", "B4", "B5"]
                .iter()
                .all(|id| s.acks().contains(&(*id).to_string())))
            .await
    );
    assert!(rig.broker.decisions().is_empty());
    assert_eq!(rig.broker.attempts(), 0);
    assert!(
        ephemerals(&rig)
            .iter()
            .all(|text| text.contains("승인할 수 없어요")),
        "{:?}",
        ephemerals(&rig)
    );
    // 다른 팀의 클릭은 이 설치의 것이 아니라 받았다는 표시만 한다
    rig.slack
        .push(button_frame(
            "B6",
            "T9",
            &dm,
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"B6".to_string()))
            .await
    );
    assert_eq!(rig.broker.attempts(), 0);
}

#[tokio::test]
async fn a_click_before_the_receipt_is_confirmed_decides_nothing_and_says_to_try_again() {
    // 영수증이 아직 없다(카드가 열리지 않았다): 일반 DM 입력이 없으니 어떤 DM 클릭도 이 카드의 결정으로 이어지지 않는다. 누른 사람에게만 알린다
    let rig = connected(vec![channel_only()]).await;
    rig.ledger
        .put(NONCE, "personal", ApprovalState::Pending, &card_json());
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "D1",
            "U1",
            "1700000000.000001",
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(rig.broker.attempts(), 0);
    let notices = ephemerals(&rig);
    assert!(
        notices.len() == 1 && notices[0].contains("아직 열리지 않은"),
        "{notices:?}"
    );
    assert_eq!(rig.slack.calls("chat.postEphemeral")[0].body["user"], "U1");
}

#[tokio::test]
async fn a_click_on_a_closed_dm_card_is_explained_to_its_owner_and_decides_nothing() {
    // 전달 실패로 닫힌 요청의 카드가 DM에 남아 있는 경우(게시 여부가 불명이었다): 누른 구성원에게만 «닫혔어요»를 알리고 결정하지 않는다
    let rig = connected(vec![channel_only()]).await;
    rig.ledger
        .put(NONCE, "personal", ApprovalState::Denied, &card_json());
    for (envelope, channel, user) in [
        ("B1", "D1", "U1"),
        // 구성원이 아니면·DM이 아닌 곳이면 아무 말도 하지 않는다
        ("B2", "D1", "U9"),
        ("B3", "C9", "U1"),
    ] {
        rig.slack
            .push(button_frame(
                envelope,
                "T1",
                channel,
                user,
                "1700000000.000001",
                "bot_approve",
                NONCE,
            ))
            .await;
    }
    assert!(
        rig.slack
            .until(|s| ["B1", "B2", "B3"]
                .iter()
                .all(|id| s.acks().contains(&(*id).to_string())))
            .await
    );
    assert_eq!(rig.broker.attempts(), 0);
    let notices = ephemerals(&rig);
    assert!(
        notices.len() == 1 && notices[0].contains("이미 닫혔어요"),
        "{notices:?}"
    );
    assert_eq!(rig.slack.calls("chat.postEphemeral")[0].body["user"], "U1");
    // 다른 프로필의 승인은 같은 DM 클릭이라도 모르는 카드다(아무 말도 하지 않는다)
    rig.ledger
        .put("n2", "work", ApprovalState::Denied, &card_json());
    rig.slack
        .push(button_frame(
            "B4",
            "T1",
            "D1",
            "U1",
            "1700000000.000001",
            "bot_approve",
            "n2",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"B4".to_string()))
            .await
    );
    assert_eq!(ephemerals(&rig).len(), 1);
}

#[tokio::test]
async fn an_approval_that_cannot_be_read_is_retried_not_decided_elsewhere() {
    // 일반 DM 입력이 열려 있어도(옛 길로 결정할 수 있어도) 승인을 읽지 못하면 영수증을 모르니 결정하지 않고 ack도 하지 않는다
    let (rig, dm, ts) = delivered(vec![personal()]).await;
    rig.ledger.fail_next(Error::Unavailable);
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            &dm,
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 1)
            .await
    );
    assert_eq!(rig.broker.attempts(), 0);
    assert!(rig.slack.acks().is_empty(), "일시 실패는 ack하지 않아요");
    // 저장소가 돌아오면 같은 클릭(재전달)이 결정된다
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            &dm,
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks() == ["B1"]).await);
    assert_eq!(rig.broker.decisions().len(), 1);
}

#[tokio::test]
async fn a_dm_card_cannot_be_decided_from_the_plain_dm_input_path_or_by_a_non_member() {
    // 일반 DM 입력이 열려 있는 프로필에서도, 영수증이 있는 카드는 그 메시지가 아닌 DM 클릭으로 결정되지 않는다
    let (rig, _dm, ts) = delivered(vec![personal()]).await;
    rig.slack
        .push(button_frame(
            "B1",
            "T1",
            "D7",
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.calls("chat.postEphemeral").len() == 1)
            .await
    );
    assert_eq!(rig.broker.attempts(), 0);
    // 소유자가 지금은 구성원이 아니면(영수증은 맞아도) 결정하지 못한다
    let (rig, dm, ts) = delivered(vec![channel_only()]).await;
    rig.catalog.replace([channel_only_without_owner().build()]);
    settle().await;
    rig.slack
        .push(button_frame(
            "B2",
            "T1",
            &dm,
            "U1",
            &ts,
            "bot_approve",
            NONCE,
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks() == ["B2"]).await);
    assert_eq!(rig.broker.attempts(), 0);
    assert!(
        ephemerals(&rig)[0].contains("등록된 사용자"),
        "{:?}",
        ephemerals(&rig)
    );
}

fn channel_only_without_owner() -> bot_gateway::testing::Fixture {
    profile("personal")
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .member("slack:home/U2", &["owner"])
        .role("owner", &["run"])
        .policy_scopes(&["run"])
}
