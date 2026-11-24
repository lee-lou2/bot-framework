//! 등록부에서 수신자가 빠졌을 때(운영자가 프로필을 고쳐 `--activate`로 다시 배포): 명시 목적지 예약은 발화 전에 이미 빠졌으면 새 실행 없이 닫힌다.
//! 실제 `claude` CLI(가짜 상류)·가짜 Slack·실제 PG. 생성 승인은 Slack 버튼으로 하고, 발화 전에 새 판본(팀 채널 바인딩 없음)을 `--activate`로 올린다.
//!
//! 실행: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-cli --test claude_route_gone` (claude CLI가 없으면 건너뛴다)
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use bot_gateway::testing::slack::{FakeSlack, dm_frame};
use claude_support::*;
use process_support::Schema;

const ROLES: [Role; 3] = [Role::Gateway, Role::Scheduler, Role::Worker];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recipient_removed_before_the_fire_time_closes_the_schedule_without_a_run_or_a_model_call()
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
        &ROLES,
        &slack.base,
    )
    .await;
    assert!(slack.connected().await, "Slack 소켓에 붙지 않았어요");

    // 소유자의 DM에서 팀 채널로 가는 예약을 만들고 생성 승인까지 마친다(모델 응답은 둘뿐이다: 발화 run이 모델을 부르면 시험이 실패한다)
    rig.script(tool_turn(
        "msg_c1",
        "toolu_c1",
        "mcp__relay__schedule_create",
        &create_with(team_channel("planning"), &in_secs(30)),
    ));
    rig.script(final_turn("msg_c2", "예약했어요"));
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
    wait(
        &rig,
        "생성 승인 카드가 열리지 않았어요",
        |rig| rig.query(PENDING) == "1",
    )
    .await;
    approve(&slack, &rig, "B1").await;
    wait(&rig, "예약이 만들어지지 않았어요", |rig| {
        rig.query("select count(*) from {s}.bot_user_schedules where state = 'active'") == "1"
    })
    .await;
    wait(
        &rig,
        "만든 대화의 run이 끝나지 않았어요",
        |rig| rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'") == "1",
    )
    .await;
    let model_requests = rig.upstream.requests().len();

    // 운영자가 등록부에서 팀 채널을 뺀 새 판본을 올린다 → 발화 시각이 와도 새 실행은 없고 예약은 닫힌다
    rig.reactivate(&route_profile_without_team(), &ROLES).await;
    wait(&rig, "예약이 닫히지 않았어요", |rig| {
        rig.query("select state from {s}.bot_user_schedules") == "closed"
    })
    .await;
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_occurrences"),
        "0",
        "발화한 발생이 없어요"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs"),
        "1",
        "만든 대화의 run뿐이에요"
    );
    assert_eq!(rig.query(PENDING), "0", "전송 승인 카드도 열리지 않았어요");
    assert_eq!(
        rig.query("select count(*) from {s}.bot_outbox where conv_id like '%C1'"),
        "0"
    );
    assert!(
        posts_to(&slack, "C1").is_empty(),
        "팀 채널로는 아무것도 가지 않았어요"
    );
    assert_eq!(
        rig.upstream.requests().len(),
        model_requests,
        "모델을 다시 부르지 않았어요"
    );
    rig.stop().await;
}

// 재배포 복원(`serve/restore.rs`)이 옛 판본을 되살리고 ②의 PG `ScheduledReplies::finish_unsent`가 수신자 철회를 «원래 대화 고정 안내 한 번·목적지 outbox 0·정상 종료»로 끝낸다.
// 복원이 없던 때는 `--activate` 직후 감사가 `approval.supersede` → `run.dead:delivery_missing`으로 끝나(옛 카드를 누르기도 전에 run이 죽는다) 이 시험이 `--ignored`였다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recipient_removed_after_the_draft_card_opened_ends_the_occurrence_with_one_notice_and_sends_nothing()
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
        &ROLES,
        &slack.base,
    )
    .await;
    assert!(slack.connected().await, "Slack 소켓에 붙지 않았어요");

    // 생성 승인 → 발화 → 전송 초안이 동결돼 두 번째 승인 카드가 열린다
    rig.script(tool_turn(
        "msg_c1",
        "toolu_c1",
        "mcp__relay__schedule_create",
        &create_with(team_channel("planning"), &in_secs(20)),
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
    wait(
        &rig,
        "생성 승인 카드가 열리지 않았어요",
        |rig| rig.query(PENDING) == "1",
    )
    .await;
    approve(&slack, &rig, "B1").await;
    wait(
        &rig,
        "전송 초안 승인 카드가 열리지 않았어요",
        |rig| {
            rig.query("select count(*) from {s}.bot_deliveries where state = 'drafted'") == "1"
                && rig.query(PENDING) == "1"
        },
    )
    .await;
    let nonce = rig.query("select nonce from {s}.bot_approvals where state = 'pending'");
    let card = posted_card(&slack, &nonce).await;
    let run = rig.query("select run from {s}.bot_deliveries where state = 'drafted'");
    let model_requests = rig.upstream.requests().len();

    // 카드가 열린 뒤 운영자가 등록부에서 팀 채널을 뺀 새 판본을 올린다 → 새 게이트웨이가 Slack에 다시 붙는다
    rig.reactivate(&route_profile_without_team(), &ROLES).await;
    assert!(
        slack.until(|slack| slack.connects() >= 2).await,
        "새 게이트웨이가 Slack에 붙지 않았어요"
    );
    // 소유자가 옛 카드의 승인 버튼을 누른다 → 이번 발생만 보내지 않고 끝낸다: 원래 대화(DM)에 고정 안내 한 번, 목적지 outbox 0, run은 정상 종료
    click(&slack, "B2", &card).await;
    wait(&rig, "발생이 끝나지 않았어요", |rig| {
        rig.query(&format!(
            "select state from {{s}}.bot_runs where id = '{run}'"
        )) == "succeeded"
    })
    .await;
    let notices = format!(
        "select count(*) from {{s}}.bot_outbox where run = '{run}' and conv_id = 'D1' and body like '%이번 답은 보내지 않았습니다%'"
    );
    assert_eq!(rig.query(&notices), "1", "{}", dump(&rig));
    assert_eq!(
        rig.query("select count(*) from {s}.bot_outbox where conv_id like '%C1'"),
        "0",
        "목적지 outbox는 비어 있어요"
    );
    assert_eq!(rig.query(PENDING), "0", "옛 카드는 더는 열려 있지 않아요");
    wait(
        &rig,
        "고정 안내가 Slack에 가지 않았어요",
        |_| {
            posts_to(&slack, "D1").iter().any(|call| {
                call.body["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("이번 답은 보내지 않았습니다"))
            })
        },
    )
    .await;
    assert!(
        posts_to(&slack, "C1").is_empty(),
        "팀 채널로는 아무것도 가지 않았어요"
    );

    // 같은 버튼을 다시 눌러도 안내는 늘지 않고, 모델도 다시 부르지 않는다
    click(&slack, "B3", &card).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(rig.query(&notices), "1", "안내는 한 번뿐이에요");
    assert_eq!(rig.upstream.requests().len(), model_requests);
    rig.stop().await;
}
