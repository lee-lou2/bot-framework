//! 채널에서 시작한 예약의 승인 카드는 소유자의 1:1 DM으로 간다 — 종단 수락 시험(가짜 Slack DM·실제 PG·실제 `claude` CLI(가짜 상류)).
//! 일반 DM 입력 바인딩이 없는 프로필에서도 카드의 버튼만 동작하고, 원래 채널에는 «DM으로 승인 요청을 보냈어요» 한 줄만 남는다.
//! DM을 열 수 없거나 게시 응답을 잃으면 승인도 예약도 성공 안내도 목적지 발송도 없고, 원래 채널·스레드에는 닫혔다는 한 줄만 남는다.
//!
//! 실행: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-cli --test claude_card_dm` (claude CLI가 없으면 건너뛴다)
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use bot_gateway::testing::slack::{Call, FakeSlack, Scripted, button_frame, mention_frame};
use claude_support::*;
use process_support::Schema;
use serde_json::json;
use std::time::Duration;

const MENTION: &str = "<@UBOT> 자료가 오면 팀 채널에 알려 줘";
const ROLES: [Role; 3] = [Role::Gateway, Role::Scheduler, Role::Worker];

fn create_turns(rig: &Rig, n: u32, after: u64) {
    rig.script(tool_turn(
        &format!("msg_c{n}"),
        &format!("toolu_c{n}"),
        "mcp__relay__schedule_create",
        &create_with(team_channel("planning"), &in_secs(after)),
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_started_schedule_asks_the_owner_in_a_dm_and_only_that_card_decides_it() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let slack = FakeSlack::start().await;
    let mut rig = Rig::start_slack(
        schema,
        claude,
        CARD_PROFILE,
        &card_gateway(),
        &ROLES,
        &slack.base,
    )
    .await;
    assert!(slack.connected().await, "Slack 소켓에 붙지 않았어요");

    // 1) 원래 채널(ops, C2)에서 소유자가 예약을 시작한다. 모델은 등록된 팀 채널을 목적지로 준다
    create_turns(&rig, 1, 25);
    rig.script(final_turn("msg_c2", "예약했어요"));
    rig.script(final_turn("msg_f1", "자료가 도착했어요"));
    let first = ts(1);
    slack
        .push(mention_frame("E1", "T1", "C2", "U1", &first, MENTION))
        .await;
    rig.until("생성 승인이 열리지 않았어요", 90, |rig| {
        rig.query(PENDING) == "1"
    })
    .await;
    let nonce = pending_nonce(&rig);

    // 2) 전체 카드는 소유자의 DM(D1)에만 올라가고 원래 채널에는 아무 카드도 가지 않는다
    let card = posted_card(&slack, &nonce).await;
    assert_eq!(card.channel, "D1", "카드는 소유자 DM으로 가요");
    assert_eq!(slack.calls("conversations.open")[0].body["users"], "U1");
    let dm_post = slack
        .calls("chat.postMessage")
        .into_iter()
        .find(|call| call.body["channel"] == "D1")
        .unwrap();
    assert!(dm_post.body.get("thread_ts").is_none_or(|t| t.is_null()));
    assert!(has_buttons(&dm_post));
    let body = dm_post.body["text"].as_str().unwrap();
    assert!(body.contains("T1/C1"), "{body}");

    // 3) 게시가 확정되면 카드가 열리고 원래 채널·스레드에 안내 한 줄만 간다(초안·수신자·버튼·승인 식별자 없음)
    rig.until("카드 전달이 확정되지 않았어요", 60, |rig| {
        job_state(rig, &nonce) == "posted"
    })
    .await;
    rig.until("원래 채널 안내가 나가지 않았어요", 60, |_| {
        !texts_to(&slack, "C2").is_empty()
    })
    .await;
    let notice = posts_to(&slack, "C2");
    assert_eq!(notice.len(), 1, "{}", dump(&rig));
    assert_eq!(notice[0].body["text"], DM_NOTICE);
    assert_eq!(notice[0].body["thread_ts"], first.as_str());
    assert!(!has_buttons(&notice[0]));
    assert!(posts_to(&slack, "C1").is_empty());

    // 4) 이 카드가 아닌 곳·사람·메시지의 클릭은 승인을 소비하지 않는다(복사한 채널·다른 DM·다른 메시지·다른 사용자·다른 팀)
    let mut wrong = Vec::new();
    for (envelope, channel, user, message_ts, team) in [
        ("X1", "C2", "U1", card.ts.as_str(), "T1"),
        ("X2", "D9", "U1", card.ts.as_str(), "T1"),
        ("X3", "D1", "U1", "1700000000.999999", "T1"),
        ("X4", "D1", "U2", card.ts.as_str(), "T1"),
        ("X5", "D1", "U1", card.ts.as_str(), "T9"),
    ] {
        wrong.push(envelope);
        slack
            .push(button_frame(
                envelope,
                team,
                channel,
                user,
                message_ts,
                "bot_approve",
                &card.value,
            ))
            .await;
    }
    assert!(
        slack
            .until(|s| wrong.iter().all(|id| s.acks().contains(&(*id).to_string())))
            .await,
        "영구적으로 처리할 수 없는 클릭은 모두 ack해요"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(rig.query(PENDING), "1", "{}", dump(&rig));
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_audit where action like 'approval.decide%'"),
        "0"
    );

    // 5) 소유자가 DM 카드를 누르면 예약이 하나 만들어진다(같은 클릭의 재전달은 결정을 늘리지 않는다)
    click(&slack, "B1", &card).await;
    rig.until("예약이 만들어지지 않았어요", 90, |rig| {
        rig.query("select count(*) from {s}.bot_user_schedules where state = 'active'") == "1"
    })
    .await;
    assert_eq!(
        rig.query(
            "select dest_binding || '|' || dest_kind || '|' || dest_id || '|' || origin_binding || '|' || origin_id \
             from {s}.bot_user_schedules"
        ),
        "team|channel|T1/C1|ops|C2",
        "{}",
        dump(&rig)
    );
    click(&slack, "B1-again", &card).await;
    assert!(
        slack
            .until(|s| s.acks().contains(&"B1-again".to_string()))
            .await
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "1"
    );

    // 6) 발화: 전송 초안 승인도 소유자 DM으로 온다(새 카드·새 메시지). 목적지에는 아직 아무것도 가지 않았다
    rig.until(
        "전송 초안이 동결돼 승인을 기다리지 않아요",
        90,
        |rig| {
            rig.query("select count(*) from {s}.bot_deliveries where state = 'drafted'") == "1"
                && rig.query(PENDING) == "1"
        },
    )
    .await;
    let draft_nonce = pending_nonce(&rig);
    assert_ne!(draft_nonce, nonce);
    let draft = posted_card(&slack, &draft_nonce).await;
    assert_eq!(draft.channel, "D1");
    assert_ne!(draft.ts, card.ts);
    assert!(
        posts_to(&slack, "C1").is_empty(),
        "승인 전에는 목적지로 아무것도 가지 않아요"
    );
    // 앞 카드의 메시지로 새 승인을 결정할 수 없다
    let stale = PostedCard {
        channel: card.channel.clone(),
        ts: card.ts.clone(),
        value: draft.value.clone(),
    };
    click(&slack, "X6", &stale).await;
    assert!(slack.until(|s| s.acks().contains(&"X6".to_string())).await);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(rig.query(PENDING), "1", "{}", dump(&rig));

    // 7) 새 카드를 승인하면 목적지(C1)에 정확히 한 번 간다
    click(&slack, "B2", &draft).await;
    rig.until("승인한 초안이 전송되지 않았어요", 90, |rig| {
        rig.query("select state from {s}.bot_outbox where conv_id = 'T1/C1'") == "confirmed"
    })
    .await;
    assert_eq!(posts_to(&slack, "C1").len(), 1, "{}", dump(&rig));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(posts_to(&slack, "C1").len(), 1, "다시 보내지 않아요");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dm_that_cannot_be_opened_or_a_lost_post_closes_the_request_and_says_so_once_in_the_channel()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let slack = FakeSlack::start().await;
    let rig = Rig::start_slack(
        schema,
        claude,
        CARD_PROFILE,
        &card_gateway(),
        &ROLES,
        &slack.base,
    )
    .await;
    assert!(slack.connected().await, "Slack 소켓에 붙지 않았어요");
    let closed = "select count(*) from {s}.bot_audit where action = 'approval.dm_unavailable' and verdict = 'approval_dm_unavailable'";

    // A) 소유자 DM을 열 수 없다(확정 거절): 승인은 닫히고 예약·카드·성공 안내·목적지 발송이 0이며 원래 스레드에 «보낼 수 없어 닫았어요» 한 줄이 간다
    slack.script(
        "conversations.open",
        Scripted::Json(json!({"ok": false, "error": "user_not_found"})),
    );
    create_turns(&rig, 1, 3600);
    rig.script(final_turn(
        "msg_c2",
        "DM으로 승인을 보낼 수 없어 예약하지 못했어요",
    ));
    let (first, second) = (ts(1), ts(2));
    slack
        .push(mention_frame("E1", "T1", "C2", "U1", &first, MENTION))
        .await;
    wait(&rig, "첫 요청이 끝나지 않았어요", |rig| {
        rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'") == "1"
    })
    .await;
    assert_eq!(rig.query(closed), "1", "{}", dump(&rig));
    assert_eq!(rig.query(PENDING), "0");
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    assert!(
        slack
            .calls("chat.postMessage")
            .iter()
            .all(|call| call.body["channel"] != "D1"),
        "열리지 않은 DM에는 아무것도 올라가지 않아요"
    );
    // 원래 채널·스레드에는 닫혔다는 한 줄만 간다(초안·수신자·승인 식별자·버튼 없음). 성공 안내와 «확인하지 못했다» 안내는 없다
    wait(&rig, "닫힘 안내가 나가지 않았어요", |_| {
        !said_in(&slack, "C2", DM_UNAVAILABLE_NOTICE).is_empty()
    })
    .await;
    let lines = said_in(&slack, "C2", DM_UNAVAILABLE_NOTICE);
    assert_eq!(lines.len(), 1, "{}", dump(&rig));
    assert_eq!(lines[0].body["thread_ts"], first.as_str());
    assert!(!has_buttons(&lines[0]));
    assert!(
        said_in(&slack, "C2", DM_NOTICE).is_empty(),
        "성공 안내는 없어요"
    );
    assert!(said_in(&slack, "C2", DM_UNCONFIRMED_NOTICE).is_empty());
    assert!(posts_to(&slack, "C1").is_empty());

    // B) 카드는 올렸는데 응답을 잃었다(게시 여부 불명): 자동으로 다시 올리지 않고 닫는다.
    // A의 회신(닫힘 안내와 모델의 마지막 말)이 먼저 모두 나간 뒤에 이 응답이 쓰이도록 기다린다
    wait(&rig, "첫 회신이 나가지 않았어요", |_| {
        texts_to(&slack, "C2").len() == 2
    })
    .await;
    slack.script(
        "chat.postMessage",
        Scripted::Raw {
            status: 502,
            retry_after: None,
            body: String::new(),
        },
    );
    create_turns(&rig, 3, 3600);
    rig.script(final_turn(
        "msg_c4",
        "응답을 확인하지 못해 예약하지 못했어요",
    ));
    slack
        .push(mention_frame("E2", "T1", "C2", "U1", &second, MENTION))
        .await;
    wait(&rig, "둘째 요청이 끝나지 않았어요", |rig| {
        rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'") == "2"
    })
    .await;
    assert_eq!(rig.query(closed), "2", "{}", dump(&rig));
    let attempts = slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|call| call.body["channel"] == "D1")
        .count();
    assert_eq!(attempts, 1, "카드를 다시 올리지 않아요");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        slack
            .calls("chat.postMessage")
            .into_iter()
            .filter(|call| call.body["channel"] == "D1")
            .count(),
        1
    );
    assert_eq!(rig.query(PENDING), "0");
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_card_jobs where state = 'closed'"),
        "2"
    );
    // 이번에는 «전달됐는지 확인하지 못해 닫았어요» 한 줄이 둘째 스레드에 한 번 간다(앞의 안내는 그대로 한 번)
    wait(
        &rig,
        "확인 못 함 안내가 나가지 않았어요",
        |_| !said_in(&slack, "C2", DM_UNCONFIRMED_NOTICE).is_empty(),
    )
    .await;
    let unsure = said_in(&slack, "C2", DM_UNCONFIRMED_NOTICE);
    assert_eq!(unsure.len(), 1, "{}", dump(&rig));
    assert_eq!(unsure[0].body["thread_ts"], second.as_str());
    assert_eq!(said_in(&slack, "C2", DM_UNAVAILABLE_NOTICE).len(), 1);
    assert!(
        said_in(&slack, "C2", DM_NOTICE).is_empty(),
        "성공 안내는 없어요"
    );
    assert!(posts_to(&slack, "C1").is_empty());

    // C) B의 카드가 실제로는 DM에 올라가 있었다고 하자(게시 여부가 불명이었다): 그 버튼을 눌러도 결정되지 않고 소유자에게만 «닫혔어요»가 보인다
    let stale = rig.query(
        "select nonce from {s}.bot_card_jobs where state = 'closed' order by created_ms desc limit 1",
    );
    slack
        .push(button_frame(
            "S1",
            "T1",
            "D1",
            "U1",
            "1700000000.000009",
            "bot_approve",
            &stale,
        ))
        .await;
    assert!(slack.until(|s| s.acks().contains(&"S1".to_string())).await);
    let notices = slack.calls("chat.postEphemeral");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].body["user"], "U1");
    assert!(
        notices[0].body["text"]
            .as_str()
            .is_some_and(|text| text.contains("이미 닫혔어요")),
        "{}",
        notices[0].body
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    rig.stop().await;
}

/// 토큰을 바꾸는 동안(보관소에는 새 값, 실행 중인 프로세스의 기억에는 옛 값) 시작된 요청의 승인 카드: 옛 토큰이 거절돼도 요청은 닫히지 않고(`approval_dm_unavailable` 아님)
/// 저장소가 백오프 뒤 다시 잡게 돌려보내 새 토큰으로 소유자 DM에 한 번 올라간다. 올라간 카드는 그대로 승인할 수 있다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dm_card_requested_while_the_bot_token_is_replaced_still_reaches_the_owner_once() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let slack = FakeSlack::start().await;
    let secrets = Rotating::new(&[
        ("model-key", "provider-key-for-personal"),
        ("bot-token", "xoxb-old"),
        ("app-token", "xapp-test-token"),
    ]);
    let mut rig = Rig::launch(
        schema,
        claude,
        CARD_PROFILE,
        &[],
        &card_gateway(),
        &ROLES,
        secrets.clone(),
        "",
        Some(&slack.base),
    )
    .await;
    rig.until(
        "serve가 활성 판본을 게시하지 않았어요",
        30,
        |rig| !rig.schema.active().is_empty(),
    )
    .await;
    assert!(slack.connected().await, "Slack 소켓에 붙지 않았어요");

    // serve는 시작할 때 옛 토큰을 확인해 기억해 두었다. 이제 토큰을 바꾼다: 보관소는 새 값, Slack는 옛 토큰을 거절한다
    secrets.set("bot-token", "xoxb-new");
    slack.revoke("xoxb-old", "invalid_auth");
    create_turns(&rig, 1, 3600);
    rig.script(final_turn("msg_c2", "예약했어요"));
    slack
        .push(mention_frame("E1", "T1", "C2", "U1", &ts(1), MENTION))
        .await;
    rig.until("생성 승인이 열리지 않았어요", 90, |rig| {
        rig.query(PENDING) == "1"
    })
    .await;
    let nonce = pending_nonce(&rig);

    // 옛 토큰이 한 번 거절됐어도 요청은 닫히지 않는다: 저장소가 다시 잡게 하고 새 토큰으로 DM에 한 번 올라간다
    rig.until("카드 전달이 확정되지 않았어요", 90, |rig| {
        job_state(rig, &nonce) == "posted"
    })
    .await;
    assert_eq!(
        rig.query("select count(*) from {s}.bot_audit where action = 'approval.dm_unavailable'"),
        "0",
        "{}",
        dump(&rig)
    );
    let tokens_of = |method: &str| -> Vec<String> {
        slack
            .calls(method)
            .into_iter()
            .map(|call| call.token)
            .collect()
    };
    // 시작 때 확인한 옛 토큰의 기억(30초)이 남아 있는 동안의 일이다. 기억이 먼저 끝났다면 옛 토큰 경로를 타지 않은 것이니 다시 돌려요
    let stale = "옛 토큰이 거절되는 경로를 타지 않았어요(시작 뒤 30초가 지났을 수 있어요)";
    assert_eq!(
        tokens_of("conversations.open"),
        ["xoxb-old", "xoxb-new"],
        "{stale}"
    );
    assert_eq!(tokens_of("auth.test"), ["xoxb-old", "xoxb-new"], "{stale}");
    let cards: Vec<Call> = slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|call| call.body["channel"] == "D1")
        .collect();
    assert_eq!(cards.len(), 1, "카드는 DM에 한 번만 올라가요");
    assert_eq!(cards[0].token, "xoxb-new");
    assert!(cards[0].posted().is_some() && has_buttons(&cards[0]));
    // 확정되면 원래 스레드에 안내 한 줄이 새 토큰으로 간다
    rig.until("원래 채널 안내가 나가지 않았어요", 60, |_| {
        !said_in(&slack, "C2", DM_NOTICE).is_empty()
    })
    .await;
    let notice = said_in(&slack, "C2", DM_NOTICE);
    assert_eq!(notice.len(), 1, "{}", dump(&rig));
    assert_eq!(notice[0].token, "xoxb-new");

    // 올라간 카드는 그대로 승인할 수 있다
    let card = posted_card(&slack, &nonce).await;
    click(&slack, "B1", &card).await;
    rig.until("예약이 만들어지지 않았어요", 90, |rig| {
        rig.query("select count(*) from {s}.bot_user_schedules where state = 'active'") == "1"
    })
    .await;
    rig.stop().await;
}
