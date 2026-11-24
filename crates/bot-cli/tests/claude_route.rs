//! 명시 목적지(다른 수신자) 예약의 종단 수락 시험 — SQL로 예약 행을 넣지 않는다. 가짜 Slack DM → 실제 `claude` CLI(가짜 상류)의
//! `schedule.create`(destination) → 생성 승인(Slack 버튼) → 발화 → 전송 초안 승인(Slack 버튼) → 목적지 outbox(`팀ID/채널ID`) →
//! 가짜 Slack 발송(채널 ID만, 정확히 한 번). 호스트 `ScheduleRoutes`(worker·결정 브로커·scheduler가 같은 등록부)가 실제로 연결돼 있어야 통과한다.
//!
//! 실행: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-cli --test claude_route` (claude CLI가 없으면 건너뛴다)
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use bot_gateway::testing::slack::{FakeSlack, dm_frame};
use claude_support::*;
use process_support::Schema;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conversation_schedules_a_reply_to_the_registered_channel_through_two_owner_approvals_and_it_is_sent_once_by_channel_id()
 {
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
        ROUTE_PROFILE,
        &route_gateway(),
        &[Role::Gateway, Role::Scheduler, Role::Worker],
        &slack.base,
    )
    .await;
    assert!(
        slack.connected().await,
        "게이트웨이가 Slack 소켓에 붙지 않았어요"
    );

    // 1) 생성: 소유자의 DM에서 모델이 `schedule.create`에 등록된 팀 채널을 목적지로 준다(별칭 `planning`)
    rig.script(tool_turn(
        "msg_c1",
        "toolu_c1",
        "mcp__relay__schedule_create",
        &create_with(team_channel("planning"), &in_secs(25)),
    ));
    rig.script(final_turn("msg_c2", "예약했어요"));
    rig.script(final_turn("msg_f1", "자료가 도착했어요"));
    slack
        .push(dm_frame(
            "E1",
            "T1",
            "D1",
            "U1",
            &ts(1),
            "자료가 오면 팀 채널에 알려 줘",
        ))
        .await;
    rig.until("생성 승인 카드가 열리지 않았어요", 90, |rig| {
        rig.query(PENDING) == "1"
    })
    .await;
    // 승인 전에는 예약이 없다. 카드는 호스트가 확정한 정규 수신자와 지시문을 그대로 보여 준다
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    let card = rig.query("select card from {s}.bot_approvals where state = 'pending'");
    assert!(
        card.contains("T1/C1") && card.contains("자료 도착을 팀에 알려 줘"),
        "{card}"
    );

    // 2) 소유자가 생성을 승인한다 → 예약 하나. 저장된 목적지는 등록부가 확정한 `팀ID/채널ID`다(원래 대화는 DM으로 따로 남는다)
    approve(&slack, &rig, "B1").await;
    rig.until("예약이 만들어지지 않았어요", 90, |rig| {
        rig.query("select count(*) from {s}.bot_user_schedules where state = 'active'") == "1"
    })
    .await;
    assert_eq!(
        rig.query(
            "select dest_binding || '|' || dest_kind || '|' || dest_id || '|' || origin_binding || '|' || origin_id \
             from {s}.bot_user_schedules"
        ),
        "team|channel|T1/C1|dm|D1"
    );

    // 3) 발화: 이 run의 최종 답은 초안으로 동결되고 두 번째 승인을 기다린다 — 목적지에는 아직 아무것도 가지 않았다
    rig.until(
        "전송 초안이 동결돼 승인을 기다리지 않아요",
        90,
        |rig| {
            rig.query("select count(*) from {s}.bot_deliveries where state = 'drafted'") == "1"
                && rig.query(PENDING) == "1"
        },
    )
    .await;
    let card = rig.query("select card from {s}.bot_approvals where state = 'pending'");
    assert!(
        card.contains("전송 초안") && card.contains("자료가 도착했어요") && card.contains("T1/C1"),
        "{card}"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_outbox where conv_id like '%C1'"),
        "0"
    );
    assert!(
        posts_to(&slack, "C1").is_empty(),
        "승인 전에는 팀 채널로 아무것도 가지 않아요"
    );
    let model_requests = rig.upstream.requests().len();

    // 4) 소유자가 전송 초안을 승인한다 → 목적지 outbox 한 건 → 가짜 Slack에는 **채널 ID만** 간다
    approve(&slack, &rig, "B2").await;
    rig.until("승인한 초안이 전송되지 않았어요", 90, |rig| {
        rig.query("select state from {s}.bot_outbox where conv_id = 'T1/C1'") == "confirmed"
    })
    .await;
    assert_eq!(
        rig.query(
            "select binding || '|' || conv_kind || '|' || conv_id from {s}.bot_outbox where conv_id like '%C1'"
        ),
        "team|channel|T1/C1",
        "목적지 outbox는 한 건이고 정규 경로가 그대로 쌓인다"
    );
    let sent = posts_to(&slack, "C1");
    assert_eq!(sent.len(), 1, "팀 채널 발송은 정확히 한 번이에요");
    assert!(
        sent[0].body["text"]
            .as_str()
            .is_some_and(|text| text.contains("자료가 도착했어요")),
        "{}",
        sent[0].body
    );
    assert!(
        sent[0].body.get("thread_ts").is_none_or(|t| t.is_null()),
        "스레드를 지정하지 않았어요"
    );
    assert_eq!(
        rig.upstream.requests().len(),
        model_requests,
        "승인 뒤 재개는 모델을 다시 부르지 않아요"
    );
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(posts_to(&slack, "C1").len(), 1, "다시 보내지 않아요");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unregistered_destination_or_one_named_from_a_cli_conversation_creates_nothing() {
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
        ROUTE_PROFILE,
        &route_gateway(),
        &[Role::Gateway, Role::Scheduler, Role::Worker],
        &slack.base,
    )
    .await;
    assert!(slack.connected().await, "Slack 소켓에 붙지 않았어요");
    let refusals = |word: &str| {
        format!(
            "select count(*) from {{s}}.bot_audit where action = 'schedule.refuse' and verdict = '{word}'"
        )
    };

    // A) Slack DM: 이 프로필의 팀 바인딩이 받는 채널이 아닌 곳(`ops`)·다른 팀의 정규 경로는 등록되지 않은 수신자다 — 카드도 예약도 없다
    for (n, id) in ["ops", "T9/C1", "C9"].into_iter().enumerate() {
        let n = n as u32;
        rig.script(tool_turn(
            &format!("msg_a{n}"),
            &format!("toolu_a{n}"),
            "mcp__relay__schedule_create",
            &create_with(team_channel(id), &in_secs(3600)),
        ));
        rig.script(final_turn(&format!("msg_b{n}"), "그 수신자는 쓸 수 없대요"));
        slack
            .push(dm_frame(
                &format!("E{n}"),
                "T1",
                "D1",
                "U1",
                &ts(n + 1),
                "팀 채널에 알려 줘",
            ))
            .await;
        let want = (n + 1).to_string();
        // 거절은 «모델에 돌려준 단어»를 그 호출 자리의 표식으로 먼저 기록한 뒤 돌려주므로 첫 시도에서 정상으로 끝난다: 모델 요청은 메시지마다 둘뿐이고 거절 감사는 하나다
        wait(&rig, "run이 끝나지 않았어요", |rig| {
            rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'") == want
        })
        .await;
        assert_eq!(
            rig.query(&refusals("destination_unregistered")),
            want,
            "{id}: 거절 감사는 메시지마다 하나예요: {}",
            dump(&rig)
        );
        assert_eq!(
            rig.upstream.requests().len(),
            2 * (n as usize + 1),
            "{id}: 불필요한 모델 요청이 있어요: {}",
            dump(&rig)
        );
        assert_eq!(
            rig.query("select coalesce(max(attempts), 0) from {s}.bot_runs"),
            "0",
            "{id}: 거절이 시도를 썼어요"
        );
        let refused = tool_result(&rig.upstream, &format!("toolu_a{n}")).expect("거절 결과");
        assert!(!refused.contains("\"schedule\""), "{id}: {refused}");
    }
    assert_eq!(rig.query(PENDING), "0", "승인 카드는 열리지 않았어요");
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_calls"),
        "0",
        "거절은 호출을 기록하기 전에 끝난다"
    );

    // B) CLI 대화(`bot chat`)는 원래 대화에만 답한다: 등록된 팀 채널을 줘도 거절한다
    rig.script(tool_turn(
        "msg_x1",
        "toolu_x1",
        "mcp__relay__schedule_create",
        &create_with(team_channel("planning"), &in_secs(3600)),
    ));
    rig.script(final_turn("msg_x2", "CLI에서는 다른 곳으로 못 보내요"));
    let chat = rig.chat("x1", "팀 채널에 알려 줘").await;
    assert!(chat.status.success(), "{}: {}", said(&chat), dump(&rig));
    assert_eq!(
        rig.query(&refusals("destination_unsupported")),
        "1",
        "{}",
        dump(&rig)
    );
    assert_eq!(rig.upstream.requests().len(), 8, "{}", dump(&rig));
    assert_eq!(rig.query(PENDING), "0");
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    rig.stop().await;
}
