//! Explicit-destination schedule behavior when a recipient binding is removed via `--activate`
//! before fire time or after a delivery draft approval card opens (real `claude` CLI + FakeSlack + PG).
//!
//! Run: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-cli --test claude_route_gone`
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use bot_core::schedule::UNROUTED_NOTICE;
use bot_gateway::testing::slack::{FakeSlack, dm_frame};
use claude_support::*;
use process_support::Schema;

const ROLES: [Role; 3] = [Role::Gateway, Role::Scheduler, Role::Worker];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recipient_removed_before_the_fire_time_closes_the_schedule_without_a_run_or_a_model_call()
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
        &ROLES,
        &slack.base,
    )
    .await;
    assert!(slack.connected().await, "Slack socket did not connect");

    // Only two scripted model turns: if a fired run invoked the model, the test would fail.
    rig.script(tool_turn(
        "msg_c1",
        "toolu_c1",
        "mcp__relay__schedule_create",
        &create_with(team_channel("planning"), &in_secs(30)),
    ));
    rig.script(final_turn("msg_c2", "Scheduled"));
    slack
        .push(dm_frame(
            "E1",
            "T1",
            "D1",
            "U1",
            &ts(1),
            "Notify the team channel when the report arrives",
        ))
        .await;
    wait(
        &rig,
        "schedule creation approval card did not open",
        |rig| rig.query(PENDING) == "1",
    )
    .await;
    approve(&slack, &rig, "B1").await;
    wait(&rig, "schedule was not created", |rig| {
        rig.query("select count(*) from {s}.bot_user_schedules where state = 'active'") == "1"
    })
    .await;
    wait(&rig, "origin run did not finish", |rig| {
        rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'") == "1"
    })
    .await;
    let model_requests = rig.upstream.requests().len();

    // Remove the team channel binding before the fire time; the schedule must close without firing.
    rig.reactivate(&route_profile_without_team(), &ROLES).await;
    wait(&rig, "schedule did not close", |rig| {
        rig.query("select state from {s}.bot_user_schedules") == "closed"
    })
    .await;
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_occurrences"),
        "0",
        "no occurrence should fire"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs"),
        "1",
        "only the creation run should exist"
    );
    assert_eq!(
        rig.query(PENDING),
        "0",
        "no delivery approval card should open"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_outbox where conv_id like '%C1'"),
        "0"
    );
    assert!(
        posts_to(&slack, "C1").is_empty(),
        "nothing should be posted to the team channel"
    );
    assert_eq!(
        rig.upstream.requests().len(),
        model_requests,
        "model must not be called again"
    );
    rig.stop().await;
}

// Bundle restore (`serve/restore.rs`) preserves the prior revision so approving the old draft card
// settles via `ScheduledReplies::finish_unsent` (one fixed origin notice, 0 destination outbox rows,
// succeeded run) instead of superseding the approval and killing the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recipient_removed_after_the_draft_card_opened_ends_the_occurrence_with_one_notice_and_sends_nothing()
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
        &ROLES,
        &slack.base,
    )
    .await;
    assert!(slack.connected().await, "Slack socket did not connect");

    rig.script(tool_turn(
        "msg_c1",
        "toolu_c1",
        "mcp__relay__schedule_create",
        &create_with(team_channel("planning"), &in_secs(20)),
    ));
    rig.script(final_turn("msg_c2", "Scheduled"));
    rig.script(final_turn("msg_f1", "The report has arrived"));
    slack
        .push(dm_frame(
            "E1",
            "T1",
            "D1",
            "U1",
            &ts(1),
            "Notify the team channel when the report arrives",
        ))
        .await;
    wait(
        &rig,
        "schedule creation approval card did not open",
        |rig| rig.query(PENDING) == "1",
    )
    .await;
    approve(&slack, &rig, "B1").await;
    wait(&rig, "delivery draft approval card did not open", |rig| {
        rig.query("select count(*) from {s}.bot_deliveries where state = 'drafted'") == "1"
            && rig.query(PENDING) == "1"
    })
    .await;
    let nonce = rig.query("select nonce from {s}.bot_approvals where state = 'pending'");
    let card = posted_card(&slack, &nonce).await;
    let run = rig.query("select run from {s}.bot_deliveries where state = 'drafted'");
    let model_requests = rig.upstream.requests().len();

    rig.reactivate(&route_profile_without_team(), &ROLES).await;
    assert!(
        slack.until(|slack| slack.connects() >= 2).await,
        "new gateway did not connect to Slack"
    );
    click(&slack, "B2", &card).await;
    wait(&rig, "occurrence did not finish", |rig| {
        rig.query(&format!(
            "select state from {{s}}.bot_runs where id = '{run}'"
        )) == "succeeded"
    })
    .await;
    let notices = format!(
        "select count(*) from {{s}}.bot_outbox where run = '{run}' and conv_id = 'D1' and body like '%{UNROUTED_NOTICE}%'"
    );
    assert_eq!(rig.query(&notices), "1", "{}", dump(&rig));
    assert_eq!(
        rig.query("select count(*) from {s}.bot_outbox where conv_id like '%C1'"),
        "0",
        "destination outbox must be empty"
    );
    assert_eq!(
        rig.query(PENDING),
        "0",
        "old card must no longer be pending"
    );
    wait(&rig, "fixed notice was not posted to Slack", |_| {
        posts_to(&slack, "D1").iter().any(|call| {
            call.body["text"]
                .as_str()
                .is_some_and(|text| text.contains(UNROUTED_NOTICE))
        })
    })
    .await;
    assert!(
        posts_to(&slack, "C1").is_empty(),
        "nothing should be posted to the team channel"
    );

    // Clicking the same approval button again must be idempotent with no extra notice or model call.
    click(&slack, "B3", &card).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(rig.query(&notices), "1", "notice must be sent only once");
    assert_eq!(rig.upstream.requests().len(), model_requests);
    rig.stop().await;
}
