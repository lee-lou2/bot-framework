//! End-to-end acceptance tests for explicit-destination chat schedules using real `claude` CLI, FakeSlack, and PG.
//! Exercises `schedule.create(destination)` -> create approval -> fire -> delivery draft approval -> destination outbox
//! (`team/channel`) -> single Slack send by channel ID, with shared host `ScheduleRoutes` across worker, broker, and scheduler.
//!
//! Run: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-cli --test claude_route`
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
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
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
        "gateway did not connect to FakeSlack socket"
    );

    // 1) Creation: model targets the registered team channel alias `planning` from the owner's DM.
    let create_args = create_with(team_channel("planning"), &in_secs(25));
    let instruction =
        serde_json::from_str::<serde_json::Value>(&create_args).unwrap()["instruction"]
            .as_str()
            .unwrap()
            .to_owned();
    rig.script(tool_turn(
        "msg_c1",
        "toolu_c1",
        "mcp__relay__schedule_create",
        &create_args,
    ));
    rig.script(final_turn("msg_c2", "Scheduled"));
    rig.script(final_turn("msg_f1", "Data has arrived"));
    slack
        .push(dm_frame(
            "E1",
            "T1",
            "D1",
            "U1",
            &ts(1),
            "Notify the team channel when the data arrives",
        ))
        .await;
    rig.until("create approval card did not open", 90, |rig| {
        rig.query(PENDING) == "1"
    })
    .await;
    // Before approval, no schedule exists and the card displays the canonical recipient and instruction.
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    let card = rig.query("select card from {s}.bot_approvals where state = 'pending'");
    assert!(
        card.contains("T1/C1") && card.contains(&instruction),
        "{card}"
    );

    // 2) Owner approves creation -> schedule stores canonical `team/channel` destination while preserving DM origin.
    approve(&slack, &rig, "B1").await;
    rig.until("schedule was not created", 90, |rig| {
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

    // 3) Fire: final reply freezes as a delivery draft awaiting a second approval; nothing reaches the destination yet.
    rig.until(
        "delivery draft did not freeze and await approval",
        90,
        |rig| {
            rig.query("select count(*) from {s}.bot_deliveries where state = 'drafted'") == "1"
                && rig.query(PENDING) == "1"
        },
    )
    .await;
    let card = rig.query("select card from {s}.bot_approvals where state = 'pending'");
    assert!(
        card.contains("Delivery draft")
            && card.contains("Data has arrived")
            && card.contains("T1/C1"),
        "{card}"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_outbox where conv_id like '%C1'"),
        "0"
    );
    assert!(
        posts_to(&slack, "C1").is_empty(),
        "nothing may be posted to the team channel before approval"
    );
    let model_requests = rig.upstream.requests().len();

    // 4) Owner approves the draft -> one destination outbox row -> Slack receives only the bare channel ID.
    approve(&slack, &rig, "B2").await;
    rig.until("approved draft was not delivered", 90, |rig| {
        rig.query("select state from {s}.bot_outbox where conv_id = 'T1/C1'") == "confirmed"
    })
    .await;
    assert_eq!(
        rig.query(
            "select binding || '|' || conv_kind || '|' || conv_id from {s}.bot_outbox where conv_id like '%C1'"
        ),
        "team|channel|T1/C1",
        "destination outbox must record one row with the canonical route"
    );
    let sent = posts_to(&slack, "C1");
    assert_eq!(sent.len(), 1, "team channel must receive exactly one post");
    assert!(
        sent[0].body["text"]
            .as_str()
            .is_some_and(|text| text.contains("Data has arrived")),
        "{}",
        sent[0].body
    );
    assert!(
        sent[0].body.get("thread_ts").is_none_or(|t| t.is_null()),
        "no thread_ts was requested"
    );
    assert_eq!(
        rig.upstream.requests().len(),
        model_requests,
        "resuming after approval must not invoke the model again"
    );
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(posts_to(&slack, "C1").len(), 1, "must not redeliver");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unregistered_destination_or_one_named_from_a_cli_conversation_creates_nothing() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
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
    assert!(slack.connected().await, "gateway did not connect to Slack");
    let refusals = |word: &str| {
        format!(
            "select count(*) from {{s}}.bot_audit where action = 'schedule.refuse' and verdict = '{word}'"
        )
    };

    // A) Slack DM: channels outside this profile's team binding (`ops`, `T9/C1`, `C9`) are refused without cards or schedules.
    for (n, id) in ["ops", "T9/C1", "C9"].into_iter().enumerate() {
        let n = n as u32;
        rig.script(tool_turn(
            &format!("msg_a{n}"),
            &format!("toolu_a{n}"),
            "mcp__relay__schedule_create",
            &create_with(team_channel(id), &in_secs(3600)),
        ));
        rig.script(final_turn(
            &format!("msg_b{n}"),
            "That recipient is not available",
        ));
        slack
            .push(dm_frame(
                &format!("E{n}"),
                "T1",
                "D1",
                "U1",
                &ts(n + 1),
                "Notify the team channel",
            ))
            .await;
        let want = (n + 1).to_string();
        // Refusals return a fixed code immediately without consuming retry attempts: 2 model turns and 1 refusal audit per message.
        wait(&rig, "run did not finish", |rig| {
            rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'") == want
        })
        .await;
        assert_eq!(
            rig.query(&refusals("destination_unregistered")),
            want,
            "{id}: expected one refusal audit per message: {}",
            dump(&rig)
        );
        assert_eq!(
            rig.upstream.requests().len(),
            2 * (n as usize + 1),
            "{id}: unexpected extra model requests: {}",
            dump(&rig)
        );
        assert_eq!(
            rig.query("select coalesce(max(attempts), 0) from {s}.bot_runs"),
            "0",
            "{id}: refusal must not spend retry attempts"
        );
        let refused = tool_result(&rig.upstream, &format!("toolu_a{n}")).expect("refusal result");
        assert!(!refused.contains("\"schedule\""), "{id}: {refused}");
    }
    assert_eq!(rig.query(PENDING), "0", "no approval card may open");
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules"),
        "0"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_calls"),
        "0",
        "refusal happens before recording a call"
    );

    // B) CLI conversations reply only in place; even a registered team channel is refused.
    rig.script(tool_turn(
        "msg_x1",
        "toolu_x1",
        "mcp__relay__schedule_create",
        &create_with(team_channel("planning"), &in_secs(3600)),
    ));
    rig.script(final_turn(
        "msg_x2",
        "Cannot send to another destination from CLI",
    ));
    let chat = rig.chat("x1", "Notify the team channel").await;
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
