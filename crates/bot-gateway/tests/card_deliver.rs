//! 소유자 DM 승인 카드 전달 루프(`CardDeliverer`, 가짜 Slack·메모리 저장소): 작업을 점유하고 최초 전송자만 소유자 DM에 카드를 한 번 올리며, 영수증이 확정된 카드의 버튼만 클릭으로 인정한다.
//! 올라갔는지 불명이면 자동으로 다시 올리지 않고, 아무것도 올라가지 않은 일시 실패만 다시 잡는다.
mod delivery_support;
mod support;

use bot_core::{
    cards::{CardJob, CardKey, CardOutcome, CardReceipt},
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId},
    intake::NotSentReason,
    port::Error,
    profile::{Adapter, Snapshot},
};
use bot_gateway::{
    slack::{cards::is_the_card, frame::Button, socket::Installation},
    testing::{cards::MemoryCards, profile, slack::Scripted},
};
use delivery_support::*;
use serde_json::json;
use std::{sync::Arc, time::Duration};

/// 새 조립(지역 변수 `rig`가 함수 `rig`를 가리지 않게 따로 부른다)
async fn fresh() -> Rig {
    rig().await
}

fn job(nonce: &str, owner: &str, binding: &str) -> CardJob {
    CardJob {
        key: CardKey {
            nonce: ApprovalId(nonce.into()),
            attempt: 0,
        },
        profile: ProfileId("personal".into()),
        binding: BindingId(binding.into()),
        owner: PrincipalId(owner.into()),
        card: JsonPayload(
            json!({"nonce": nonce, "title": "예약 목적지 승인", "lines": ["수신자: 팀 채널"]})
                .to_string(),
        ),
        generation: Generation(1),
    }
}

fn posted(rig: &Rig) -> usize {
    rig.slack.calls("chat.postMessage").len()
}

#[tokio::test]
async fn the_first_sender_posts_the_card_to_the_owners_dm_and_the_receipt_is_settled() {
    let rig = fresh().await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    let deliverer = rig.card_deliverer(cards.clone());
    assert_eq!(deliverer.tick().await.unwrap(), 1);
    // 확인한 활성 판본으로 `begin`했고, 소유자 DM(D1)에 카드가 한 번 갔다
    assert_eq!(
        cards.begun(),
        [("n-1".to_owned(), "personal-r1".to_owned())]
    );
    assert_eq!(rig.slack.calls("conversations.open")[0].body["users"], "U1");
    assert_eq!(posted(&rig), 1);
    let sent = &rig.slack.calls("chat.postMessage")[0];
    assert_eq!(sent.body["channel"], "D1");
    let (dm, ts) = sent.posted().unwrap();
    assert_eq!(
        cards.settled(),
        [(
            "n-1".to_owned(),
            CardOutcome::Posted {
                team: "T1".into(),
                dm,
                ts
            }
        )]
    );
    // 영수증(팀·DM·ts·소유자·프로필·원래 바인딩)이 남았다
    let receipt = cards.receipt(&ApprovalId("n-1".into())).expect("영수증");
    assert_eq!(
        (
            receipt.team.as_str(),
            receipt.dm.as_str(),
            receipt.owner.0.as_str()
        ),
        ("T1", "D1", "slack:home/U1")
    );
    assert_eq!(receipt.binding.0, "team");
    // 같은 작업은 다시 잡히지 않는다
    assert_eq!(deliverer.tick().await.unwrap(), 0);
    assert_eq!(posted(&rig), 1);
}

#[tokio::test]
async fn a_dm_that_cannot_be_opened_closes_the_request_without_posting() {
    let rig = fresh().await;
    rig.slack.script(
        "conversations.open",
        Scripted::Json(json!({"ok": false, "error": "user_not_found"})),
    );
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert_eq!(
        cards.settled(),
        [("n-1".to_owned(), CardOutcome::Unavailable)]
    );
    assert_eq!(posted(&rig), 0);
    assert!(cards.receipt(&ApprovalId("n-1".into())).is_none());
}

#[tokio::test]
async fn an_unclear_post_is_held_and_never_reposted_automatically() {
    let rig = fresh().await;
    rig.slack.script(
        "chat.postMessage",
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: String::new(),
        },
    );
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    assert_eq!(cards.settled(), [("n-1".to_owned(), CardOutcome::Held)]);
    assert!(cards.receipt(&ApprovalId("n-1".into())).is_none());
    // 다음 훑기에도, 같은 식별자로 작업이 다시 들어와도 같은 승인을 두 번 올리지 않는다(`begin`은 최초 전송자만)
    deliverer.tick().await.unwrap();
    cards.push(job("n-1", "slack:home/U1", "team"));
    deliverer.tick().await.unwrap();
    assert_eq!(posted(&rig), 1, "자동 재게시 0");
}

#[tokio::test]
async fn nothing_posted_means_the_job_is_picked_up_again_and_posted_once() {
    let rig = fresh().await;
    rig.slack.script(
        "conversations.open",
        Scripted::Raw {
            status: 429,
            retry_after: Some(5),
            body: String::new(),
        },
    );
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    assert_eq!(
        cards.settled(),
        [(
            "n-1".to_owned(),
            CardOutcome::NotSent {
                reason: NotSentReason::RateLimited,
                retry_after: Some(Duration::from_secs(5))
            }
        )]
    );
    assert_eq!(posted(&rig), 0);
    assert_eq!(cards.waiting(), 1, "다시 잡히게 돌아왔어요");
    deliverer.tick().await.unwrap();
    assert_eq!(posted(&rig), 1);
    assert!(matches!(cards.settled()[1].1, CardOutcome::Posted { .. }));
}

#[tokio::test]
async fn a_settle_the_store_could_not_take_is_recorded_again_without_posting_again() {
    let rig = fresh().await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    // 올린 뒤 저장소 장애로 정산이 두 번 거절된다
    cards.fail_settle([Error::Unavailable, Error::Unavailable]);
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    assert_eq!(posted(&rig), 1);
    assert!(cards.settled().is_empty());
    deliverer.tick().await.unwrap();
    assert!(cards.settled().is_empty(), "아직 장애예요");
    // 돌아오면 같은 결과(올라간 DM·ts)가 한 번 기록되고 카드는 다시 올라가지 않는다
    deliverer.tick().await.unwrap();
    let (dm, ts) = rig.slack.calls("chat.postMessage")[0].posted().unwrap();
    assert_eq!(
        cards.settled(),
        [(
            "n-1".to_owned(),
            CardOutcome::Posted {
                team: "T1".into(),
                dm,
                ts
            }
        )]
    );
    deliverer.tick().await.unwrap();
    assert_eq!(cards.settled().len(), 1);
    assert_eq!(posted(&rig), 1);
}

#[tokio::test]
async fn a_settle_the_store_refused_for_good_is_not_retried() {
    let rig = fresh().await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    // 낡았다(`Stale`)는 답은 다시 보내도 같다
    cards.fail_settle([Error::Stale]);
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    deliverer.tick().await.unwrap();
    assert!(cards.settled().is_empty());
    assert_eq!(posted(&rig), 1);
}

#[tokio::test]
async fn a_lost_begin_or_claim_posts_nothing() {
    let rig = fresh().await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "slack:home/U1", "team"));
    // 최초 전송자 확인(`begin`)을 못 하면 올리지 않는다
    cards.fail_begin(true);
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    assert!(rig.slack.all_calls().is_empty() && cards.settled().is_empty());
    // 작업을 점유하지 못하면(저장소 장애) 오류다
    cards.fail_claim(true);
    assert!(deliverer.tick().await.is_err());
}

#[tokio::test]
async fn a_job_that_no_longer_matches_the_current_profile_is_closed_without_any_slack_call() {
    // 원래 바인딩이 이 프로필에 없다·Slack 채널 바인딩이 아니다(DM 바인딩)·소유자가 지금은 멤버가 아니다·카드가 올바르지 않다
    let mut bad_card = job("n-4", "slack:home/U1", "team");
    bad_card.card = JsonPayload("not json".into());
    let mut other_nonce = job("n-5", "slack:home/U1", "team");
    other_nonce.card = job("other", "slack:home/U1", "team").card;
    for (case, bad) in [
        ("gone", job("n-1", "slack:home/U1", "gone")),
        ("dm", job("n-2", "slack:home/U1", "direct")),
        ("stranger", job("n-3", "slack:home/U9", "team")),
        ("card", bad_card),
        ("nonce", other_nonce),
    ] {
        let rig = fresh().await;
        let cards = MemoryCards::new();
        let nonce = bad.key.nonce.clone();
        cards.push(bad);
        rig.card_deliverer(cards.clone()).tick().await.unwrap();
        assert_eq!(
            cards.settled(),
            [(nonce.0, CardOutcome::Unavailable)],
            "{case}"
        );
        assert!(rig.slack.all_calls().is_empty(), "{case}");
    }
    // 활성 프로필이 아니면 건드리지 않는다(정산도 `begin`도 없다)
    let rig = fresh().await;
    let cards = MemoryCards::new();
    let mut elsewhere = job("n-6", "slack:home/U1", "team");
    elsewhere.profile = ProfileId("nobody".into());
    cards.push(elsewhere);
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert!(cards.settled().is_empty() && cards.begun().is_empty());
    assert!(rig.slack.all_calls().is_empty());
}

fn receipt() -> CardReceipt {
    CardReceipt {
        team: "T1".into(),
        dm: "D1".into(),
        ts: "1700000000.000100".into(),
        owner: PrincipalId("slack:home/U1".into()),
        profile: ProfileId("personal".into()),
        binding: BindingId("team".into()),
    }
}

fn install() -> Installation {
    Installation {
        profile: ProfileId("personal".into()),
        alias: "home".into(),
        team: "T1".into(),
        app_id: None,
        bot_user: "UBOT".into(),
    }
}

fn click() -> Button {
    Button {
        team: "T1".into(),
        channel: "D1".into(),
        user: "U1".into(),
        message_ts: "1700000000.000100".into(),
        action_id: "bot_approve".into(),
        value: "n-1".into(),
    }
}

fn snapshot() -> Arc<Snapshot> {
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
        .binding(
            "direct",
            Adapter::SlackDm {
                workspace: "home".into(),
            },
            "main",
        )
        .binding(
            "elsewhere",
            Adapter::SlackChannel {
                workspace: "other".into(),
                channel: "general".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .member("slack:home/U1", &["owner"])
        .build()
}

#[test]
fn only_a_click_on_the_exact_confirmed_card_by_its_owner_counts() {
    let snapshot = snapshot();
    assert!(is_the_card(&receipt(), &install(), &snapshot, &click()));
    // 하나라도 다르면 그 카드의 버튼이 아니다: 메시지를 복사한 곳(채널·ts)·다른 DM·다른 사용자·다른 팀
    type Edit = fn(&mut Button);
    let clicks: [(&str, Edit); 6] = [
        ("복사한 채널", |b| b.channel = "C1".into()),
        ("다른 DM", |b| b.channel = "D2".into()),
        ("다른 메시지", |b| {
            b.message_ts = "1700000000.000200".into()
        }),
        ("ts 없음", |b| b.message_ts = String::new()),
        ("다른 사용자", |b| b.user = "U2".into()),
        ("다른 팀", |b| b.team = "T9".into()),
    ];
    for (name, edit) in clicks {
        let mut button = click();
        edit(&mut button);
        assert!(
            !is_the_card(&receipt(), &install(), &snapshot, &button),
            "{name}"
        );
    }
    // 설치(프로필·팀·별칭)가 영수증과 다르면 아니다
    type Reinstall = fn(&mut Installation);
    let installs: [(&str, Reinstall); 3] = [
        ("다른 프로필", |i| i.profile = ProfileId("work".into())),
        ("다른 팀의 설치", |i| i.team = "T9".into()),
        ("다른 설치 별칭", |i| i.alias = "other".into()),
    ];
    for (name, edit) in installs {
        let mut install = install();
        edit(&mut install);
        assert!(
            !is_the_card(&receipt(), &install, &snapshot, &click()),
            "{name}"
        );
    }
    // 영수증이 다른 팀에서 올린 것이면(클릭한 팀과 설치가 같아도) 이 설치의 카드가 아니다
    let mut other_team = receipt();
    other_team.team = "T9".into();
    assert!(!is_the_card(&other_team, &install(), &snapshot, &click()));
    // 영수증에 메시지 `ts`가 없으면(저장소 결함) 메시지 없는 클릭도 그 카드가 아니다
    let mut no_ts = receipt();
    no_ts.ts = String::new();
    let mut blank = click();
    blank.message_ts = String::new();
    assert!(!is_the_card(&no_ts, &install(), &snapshot, &blank));
    // 영수증의 소유자가 클릭한 사람이 아니면 아니다
    let mut other_owner = receipt();
    other_owner.owner = PrincipalId("slack:home/U2".into());
    assert!(!is_the_card(&other_owner, &install(), &snapshot, &click()));
    // 원래 바인딩이 지금은 이 설치의 Slack 채널 바인딩이 아니면 아니다(사라짐·DM 바인딩·다른 워크스페이스)
    for binding in ["gone", "direct", "elsewhere"] {
        let mut moved = receipt();
        moved.binding = BindingId(binding.into());
        assert!(
            !is_the_card(&moved, &install(), &snapshot, &click()),
            "{binding}"
        );
    }
}
