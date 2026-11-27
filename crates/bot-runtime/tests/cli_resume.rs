//! Acceptance tests for resuming real `claude` CLI sessions across process restarts after approval and delegation pauses (fake upstream, real PG and broker front).
//! Run: `BOT_CLAUDE_BIN=<path> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-runtime --test cli_resume`

mod support;

use bot_broker::front::park::ParkReason;
use bot_core::{id::*, port::Broker};
use bot_runtime::Outcome;
use std::{sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

#[tokio::test]
async fn an_approval_pause_resumes_the_real_cli_session_and_runs_the_tool_exactly_once() {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    script(
        &upstream,
        tool_turn(
            "msg_a1",
            "toolu_del1",
            "mcp__relay__agenda_delete",
            r#"{"event":"e1"}"#,
        ),
    );
    script(&upstream, final_turn("msg_a2", "Deletion complete"));

    let run = w.accept("req-approve", "Delete the walk event").await;
    let first = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace("a1"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("first run must complete");
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "expected pause awaiting approval: {outcome:?}"
    );
    assert_eq!(w.pg.state_of(&run).await, "waiting");
    let parked = first.park.seen();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(
        (parked[0].0.run.clone(), parked[0].1),
        (run.clone(), ParkReason::Approval)
    );
    assert_eq!(parked[0].0.generation, Generation(1));
    assert!(w.runner.jobs().is_empty());

    let keys = step_keys(&w, &run).await;
    for expected in ["cli-resp-msg_a1", "cli-use-toolu_del1"] {
        assert!(keys.iter().any(|k| k == expected), "{expected} in {keys:?}");
    }
    assert!(keys.iter().any(|k| k.starts_with("cli-rec-")), "{keys:?}");
    assert!(keys.iter().any(|k| k.starts_with("cli-group-")), "{keys:?}");
    assert_eq!(upstream.requests().len(), 1);

    let nonce =
        w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    w.broker
        .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    let old_tokens = first.tokens();
    assert!(!old_tokens.is_empty());
    drop(first);
    let second = host(
        &w,
        w.pg.peer("worker-2").await,
        upstream.clone(),
        sandbox,
        &workspace("a2"),
    )
    .await;
    // Probe while the resumed attempt holds its token: the new token works, and previous-process tokens get 401.
    let live_check = async {
        let mut live = None;
        for _ in 0..1500 {
            if let Some(token) = second.tokens().first() {
                live = Some(token.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
        let live = live.expect("resumed attempt must issue a token");
        let (status, body) = list_tools_with(second.front.addr(), &live).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains(r#""tools":["#) && body.contains("agenda_delete"),
            "live token must list tools: {body}"
        );
        for token in &old_tokens {
            assert_ne!(token, &live, "previous token must not be reused");
            let (status, body) = list_tools_with(second.front.addr(), token).await;
            assert_eq!(status, 401, "previous-process token must fail: {body}");
            assert!(!body.contains("agenda_delete"), "{body}");
        }
    };
    let (outcome, ()) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(150), second.worker.run_once()),
        live_check
    );
    let outcome = outcome.expect("resumed run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert!(second.park.seen().is_empty());
    assert_eq!(w.pg.state_of(&run).await, "succeeded");

    let jobs = w.runner.jobs();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 2);
    let text = result_text(&seen[1], "toolu_del1").expect("tool result in second request");
    assert!(text.contains("Walk"), "expected real tool result: {text}");
    assert!(
        !text.contains("approval_pending") && !text.contains("stale"),
        "{text}"
    );
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        r#"{"text":"Deletion complete"}"#
    );
    // Resumed CLI sessions only report the second process's tokens, so total usage stays null.
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select coalesce(usage_prompt::text, '-') || '/' || coalesce(usage_completion::text, '-') || '/' || coalesce(truncated::text, '-') from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        "-/-/false"
    );
}

#[tokio::test]
async fn a_child_pause_resumes_the_real_cli_session_with_the_childs_result_and_delegates_once() {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    script(
        &upstream,
        tool_turn(
            "msg_d1",
            "toolu_del2",
            "mcp__relay__delegate",
            r#"{"agent":"main/research","instruction":"Research the agenda"}"#,
        ),
    );
    script(&upstream, final_turn("msg_c1", "Research result: sunny"));
    script(&upstream, final_turn("msg_d2", "The child agent replied"));

    let run = w
        .accept("req-delegate", "Delegate to the research agent")
        .await;
    let first = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace("d1"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("parent first run must complete");
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    assert_eq!(w.pg.state_of(&run).await, "waiting");
    let parked = first.park.seen();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(
        (parked[0].0.run.clone(), parked[0].1),
        (run.clone(), ParkReason::Child)
    );
    assert_eq!(parked[0].0.generation, Generation(1));
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs where parent is not null")
            .await,
        1
    );

    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("child run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.pg.state_of(&run).await, "ready");

    drop(first);
    let second = host(
        &w,
        w.pg.peer("worker-2").await,
        upstream.clone(),
        sandbox,
        &workspace("d2"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), second.worker.run_once())
        .await
        .expect("resumed parent must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert!(second.park.seen().is_empty());
    assert_eq!(w.pg.state_of(&run).await, "succeeded");
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs where parent is not null")
            .await,
        1
    );
    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 3);
    let text = result_text(&seen[2], "toolu_del2").expect("child result in resumed parent request");
    assert!(text.contains("sunny"), "{text}");
}

#[tokio::test]
async fn two_tools_of_one_response_resume_without_rerunning_the_finished_one_and_both_results_are_real()
 {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    script(
        &upstream,
        tools_turn(
            "msg_t1",
            &[
                ("toolu_list1", "mcp__relay__agenda_list", "{}"),
                (
                    "toolu_del3",
                    "mcp__relay__agenda_delete",
                    r#"{"event":"e1"}"#,
                ),
            ],
        ),
    );
    script(&upstream, final_turn("msg_t2", "Listed and deleted"));

    let run = w
        .accept("req-two", "List the agenda and delete the walk event")
        .await;
    let first = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace("t1"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("first run must complete");
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    assert_eq!(w.pg.state_of(&run).await, "waiting");
    let parked = first.park.seen();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(
        (parked[0].0.run.clone(), parked[0].1),
        (run.clone(), ParkReason::Approval)
    );
    assert_eq!(parked[0].0.generation, Generation(1));
    let tools: Vec<String> = w
        .runner
        .jobs()
        .iter()
        .map(|j| format!("{:?}", j.tool))
        .collect();
    assert_eq!(tools.len(), 1, "only the read tool must run: {tools:?}");
    assert!(tools[0].contains("agenda.list"), "{tools:?}");

    let nonce =
        w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    w.broker
        .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    drop(first);
    let second = host(
        &w,
        w.pg.peer("worker-2").await,
        upstream.clone(),
        sandbox,
        &workspace("t2"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), second.worker.run_once())
        .await
        .expect("resumed run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert!(second.park.seen().is_empty());

    let jobs: Vec<String> = w
        .runner
        .jobs()
        .iter()
        .map(|j| format!("{:?}", j.tool))
        .collect();
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert!(jobs[1].contains("agenda.delete"), "{jobs:?}");
    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 2);
    for id in ["toolu_list1", "toolu_del3"] {
        let text = result_text(&seen[1], id).unwrap_or_else(|| panic!("missing result for {id}"));
        assert!(text.contains("Walk"), "{id}: {text}");
        assert!(
            !text.contains("approval_pending") && !text.contains("aborted"),
            "{id}: {text}"
        );
    }
}
