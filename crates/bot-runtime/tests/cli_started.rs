//! Verifies with the real `claude` CLI, PostgreSQL, and broker front that an irreversible write whose worker vanished after `call_start` (`Started`) is parked as `Held` for operator resolution without rerunning or marking refusal.
//! Operator verdicts (`AdminAct::resolve_hold`) either require fresh approval (`NotExecuted`) or complete from a receipt without rerunning (`Confirmed`).
//! Run: `BOT_CLAUDE_BIN=<path> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-runtime --test cli_started`

mod support;

use bot_broker::testing::FakeUpstream;
use bot_core::{
    admin::{AdminAct, AdminRead, HoldDecision, HoldRef, HoldVerdict, Viewer},
    id::*,
    port::Broker,
};
use bot_runtime::Outcome;
use std::{sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

const ADMIN: &str = "local:admin";

fn operator() -> Viewer {
    Viewer {
        principal: PrincipalId(ADMIN.into()),
        operator: true,
    }
}

async fn hold_of(w: &World, run: &RunId) -> HoldRef {
    let holds = AdminRead::holds(&*w.pg.store, &operator(), None)
        .await
        .unwrap();
    assert_eq!(holds.len(), 1, "{holds:?}");
    let hold = &holds[0];
    assert_eq!((&hold.run, hold.tool.0.as_str()), (run, "agenda.delete"));
    assert_eq!(hold.reason, "response_unknown");
    HoldRef {
        hold_id: hold.hold_id.clone().expect("call hold must have a hold_id"),
        profile: hold.profile.clone(),
        run: hold.run.clone(),
        call: hold.call.clone(),
        effect: None,
    }
}

async fn resolve(w: &World, run: &RunId, verdict: HoldVerdict) {
    let target = hold_of(w, run).await;
    let decided = AdminAct::resolve_hold(&*w.pg.store, PrincipalId(ADMIN.into()), target, verdict)
        .await
        .unwrap();
    assert_eq!(decided, HoldDecision::Applied);
    until("operator verdict must wake run", async || {
        w.pg.state_of(run).await == "ready"
    })
    .await;
}

async fn run_on(s: &Stranded, worker: &str, tag: &str) -> Option<Outcome> {
    let host = host(
        &s.w,
        s.w.pg.peer(worker).await,
        s.upstream.clone(),
        s.sandbox.clone(),
        &workspace(tag),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("run must finish or park")
}

struct Stranded {
    w: World,
    run: RunId,
    upstream: Arc<FakeUpstream>,
    sandbox: Arc<LocalSandbox>,
}

/// Drives a run through approval -> `Started` with a vanished worker -> takeover into `Held`.
async fn stranded() -> Option<Stranded> {
    let w = world().await?;
    let sandbox = LocalSandbox::new(claude_bin()?);
    let upstream = Arc::new(FakeUpstream::new());
    script(
        &upstream,
        tool_turn(
            "msg_s1",
            "toolu_del",
            "mcp__relay__agenda_delete",
            r#"{"event":"e1"}"#,
        ),
    );
    script(&upstream, final_turn("msg_s2", "Deletion complete"));
    let run = w.accept("req-started", "Delete the walk event").await;
    let s = Stranded {
        w,
        run: run.clone(),
        upstream,
        sandbox,
    };

    let outcome = run_on(&s, "worker-1", "st1").await;
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    s.w.pg.assert_state(&run, "waiting").await;
    assert!(s.w.runner.jobs().is_empty());
    let nonce =
        s.w.pg
            .scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    s.w.broker
        .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();

    // Hold the tool mid-flight and drop worker-2 so PG keeps `Started` with no recorded outcome.
    let _held_tool = s.w.runner.hold_next();
    let second = host(
        &s.w,
        s.w.pg.peer("worker-2").await,
        s.upstream.clone(),
        s.sandbox.clone(),
        &workspace("st2"),
    )
    .await;
    tokio::select! {
        outcome = second.worker.run_once() => panic!("worker exited while tool was held: {outcome:?}"),
        () = until("tool execution starts", async || s.w.runner.jobs().len() == 1) => {}
    }
    drop(second);
    assert_eq!(
        s.w.pg.scalar::<String>("select state from bot_calls").await,
        "started"
    );
    assert_eq!(
        s.w.pg.scalar::<String>("select state from bot_runs").await,
        "leased"
    );
    assert_eq!(
        s.w.pg.scalar::<i32>("select attempts from bot_runs").await,
        0
    );
    assert!(marks(&s.w, &run).await.is_empty());

    s.w.pg
        .sql("update bot_runs set not_before_ms = 0, lease_until_ms = case when state = 'leased' then 0 else lease_until_ms end")
        .await;
    let outcome = run_on(&s, "worker-3", "st3").await;
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    // Only the lease-expired attempt increments `attempts`; parking on a hold does not.
    assert_eq!(
        s.w.pg.scalar::<i32>("select attempts from bot_runs").await,
        1
    );
    Some(s)
}

#[tokio::test]
async fn a_write_that_started_but_never_reported_is_held_for_the_operator_and_never_rerun_or_marked()
 {
    let Some(s) = stranded().await else {
        return;
    };
    let (w, run) = (&s.w, &s.run);
    w.pg.assert_state(run, "waiting").await;
    assert_eq!(
        w.pg.scalar::<String>("select wait_kind from bot_runs")
            .await,
        "hold"
    );
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "held"
    );
    let owner = w.pg.scalar::<String>("select owner from bot_calls").await;
    assert!(owner.starts_with(&format!("{}.", run.0)), "{owner}");
    let started =
        w.pg.scalar::<i64>("select started_generation from bot_calls")
            .await;
    assert!(owner.contains(&format!(".{started}.")), "{owner}");
    assert!(
        marks(w, run).await.is_empty(),
        "started call must not be closed as refusal"
    );
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(w.runner.reaped().len(), 1, "{:?}", w.runner.reaped());
    assert_eq!(bodies(&s.upstream).len(), 1);
    hold_of(w, run).await;

    // Waking a run with an unresolved hold must park again without spending attempts.
    w.pg.sql("update bot_runs set state = 'ready', wait_kind = null, keep_generation = true")
        .await;
    let outcome = run_on(&s, "worker-4", "st4").await;
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    w.pg.assert_state(run, "waiting").await;
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "held"
    );
    assert_eq!(w.pg.scalar::<i32>("select attempts from bot_runs").await, 1);
    assert_eq!(w.runner.jobs().len(), 1);
    assert!(marks(w, run).await.is_empty());
}

#[tokio::test]
async fn an_operator_verdict_of_not_executed_runs_the_call_exactly_once_more_after_a_fresh_approval()
 {
    let Some(s) = stranded().await else {
        return;
    };
    let (w, run) = (&s.w, &s.run);
    resolve(
        w,
        run,
        HoldVerdict::NotExecuted {
            evidence: "Verified no deletion occurred at the target".into(),
        },
    )
    .await;
    // The original approval was consumed on `call_start`, so a fresh approval is required before rerunning.
    let outcome = run_on(&s, "worker-4", "st4").await;
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    w.pg.assert_state(run, "waiting").await;
    assert_eq!(
        w.pg.scalar::<String>("select wait_kind from bot_runs")
            .await,
        "approval"
    );
    assert_eq!(w.runner.jobs().len(), 1);
    let nonce =
        w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    w.broker
        .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    let outcome = run_on(&s, "worker-5", "st5").await;
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.runner.jobs().len(), 2);
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "done"
    );
    assert!(marks(w, run).await.is_empty());
    let seen = bodies(&s.upstream);
    assert_eq!(seen.len(), 2);
    let deleted = result_text(&seen[1], "toolu_del").expect("delete result");
    assert!(deleted.contains("Walk"), "{deleted}");
    assert_eq!(result_error(&seen[1], "toolu_del"), Some(false));
}

#[tokio::test]
async fn an_operator_verdict_of_executed_completes_the_call_from_the_receipt_without_running_it_again()
 {
    let Some(s) = stranded().await else {
        return;
    };
    let (w, run) = (&s.w, &s.run);
    resolve(
        w,
        run,
        HoldVerdict::Confirmed {
            receipt: "evt-7".into(),
        },
    )
    .await;
    let outcome = run_on(&s, "worker-4", "st4").await;
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "done"
    );
    assert!(marks(w, run).await.is_empty());
    let seen = bodies(&s.upstream);
    assert_eq!(seen.len(), 2);
    let receipt = result_text(&seen[1], "toolu_del").expect("receipt result");
    assert!(receipt.contains("evt-7"), "{receipt}");
    assert_eq!(result_error(&seen[1], "toolu_del"), Some(false));
}
