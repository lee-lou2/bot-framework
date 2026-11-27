//! Verifies definitive vs. tentative CLI tool refusals with the real `claude` CLI, PostgreSQL, broker front, and real `JcsSealer`+`SchemaValidator`.
//! Definitive refusals (`unknown_tool`, `invalid`) persist a PG refusal marker before replying and replay without re-judging on resume; tentative failures (unconfirmed revision, lost claim) leave no marker and re-judge on retry.
//! Run: `BOT_CLAUDE_BIN=<path> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-runtime --test cli_refusal`

mod support;

use bot_broker::{
    seal::JcsSealer,
    service::{BrokerService, Config as BrokerConfig, Parts, preflight::Preflight},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, RecordingTracer},
};
use bot_core::{
    cli::CliRefusal,
    command::JsonPayload,
    id::*,
    port::{Broker, Reply},
    profile::ActiveRevisions,
};
use bot_profile::SchemaValidator;
use bot_runtime::Outcome;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

async fn strict_world() -> Option<World> {
    world_with(Arc::new(JcsSealer::new(Arc::new(SchemaValidator::new())))).await
}

/// Promotes active revision from r1 to r2 after acceptance so listing sees r2 while the run stays pinned to r1.
async fn promote(w: &World) {
    w.catalog.publish(profile_next());
    w.pg.store
        .replace(
            Some(Revision("set-1".into())),
            Revision("set-2".into()),
            BTreeMap::from([(ProfileId("personal".into()), Revision("r2".into()))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
}

async fn audit_count(w: &World, action: &str) -> i64 {
    w.pg.scalar::<i64>(&format!(
        "select count(*) from bot_audit where action = '{action}'"
    ))
    .await
}

fn json_word(refusal: CliRefusal) -> String {
    serde_json::to_string(refusal.word()).unwrap()
}

fn assert_word(body: &serde_json::Value, id: &str, word: &str) {
    assert_eq!(
        (result_text(body, id), result_error(body, id)),
        (Some(word.to_owned()), Some(true)),
        "{id}"
    );
}

/// Preflight judge with an empty catalog so the run's pinned revision cannot be confirmed.
fn blind_judge(w: &World) -> Arc<dyn Preflight> {
    let store = w.pg.store.clone();
    Arc::new(BrokerService::new(
        Parts {
            catalog: Arc::new(FakeCatalog::new()),
            ledger: store.clone(),
            effects: store.clone(),
            memory: store.clone(),
            audit: store,
            runner: Arc::new(FakeRunner::new()),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        BrokerConfig::default(),
    ))
}

/// Simulates another worker stealing the claim right before preflight runs.
struct LosesTheClaim {
    inner: Arc<dyn Preflight>,
    pool: sqlx::PgPool,
}

impl Preflight for LosesTheClaim {
    fn refusal(
        &self,
        attempt: Attempt,
        tool: ToolId,
        args: JsonPayload,
    ) -> Reply<'_, Option<CliRefusal>> {
        Box::pin(async move {
            sqlx::query("update bot_runs set claim_epoch = claim_epoch + 1 where id = $1")
                .bind(&attempt.run.0)
                .execute(&self.pool)
                .await
                .unwrap();
            self.inner.refusal(attempt, tool, args).await
        })
    }
}

#[tokio::test]
async fn a_tool_missing_from_the_pinned_revision_and_a_schema_violation_are_stored_then_read_back_on_resume_without_running_anything()
 {
    let Some(w) = strict_world().await else {
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
            "msg_r1",
            &[
                ("toolu_extra", "mcp__relay__agenda_extra", "{}"),
                (
                    "toolu_move",
                    "mcp__relay__agenda_move",
                    r#"{"event":"Bad Id"}"#,
                ),
                (
                    "toolu_del",
                    "mcp__relay__agenda_delete",
                    r#"{"event":"e1"}"#,
                ),
            ],
        ),
    );
    script(
        &upstream,
        final_turn("msg_r2", "Skipped refused calls and deleted"),
    );
    let run = w.accept("req-refusal", "Organize my agenda").await;
    promote(&w).await;

    let first = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace("r1"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("first run must complete");
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "expected pause awaiting delete approval: {outcome:?}"
    );
    assert_eq!(w.pg.state_of(&run).await, "waiting");

    let stored = marks(&w, &run).await;
    assert_eq!(stored.len(), 2, "{stored:?}");
    let mut words: Vec<&str> = stored.iter().map(|(_, body)| body.as_str()).collect();
    words.sort();
    let mut want = vec![
        json_word(CliRefusal::UnknownTool),
        json_word(CliRefusal::Invalid),
    ];
    want.sort();
    assert_eq!(words, want, "{stored:?}");
    assert!(
        stored
            .iter()
            .all(|(key, _)| key.starts_with("cli-refusal-cli-")),
        "{stored:?}"
    );
    assert!(w.runner.jobs().is_empty());
    let judged = audit_count(&w, "call.invalid").await;
    assert_eq!(judged, 1);

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
        &workspace("r2"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), second.worker.run_once())
        .await
        .expect("resumed run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(
        audit_count(&w, "call.invalid").await,
        judged,
        "resume must replay refusal markers without re-judging via broker"
    );

    let jobs: Vec<String> = w
        .runner
        .jobs()
        .iter()
        .map(|j| format!("{:?}", j.tool))
        .collect();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert!(jobs[0].contains("agenda.delete"), "{jobs:?}");
    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 2);
    assert_word(&seen[1], "toolu_extra", "unknown_tool");
    assert_word(&seen[1], "toolu_move", "invalid");
    let deleted = result_text(&seen[1], "toolu_del").expect("delete result");
    assert!(deleted.contains("Walk"), "{deleted}");
    assert_eq!(result_error(&seen[1], "toolu_del"), Some(false));
    assert_eq!(marks(&w, &run).await, stored);
}

#[derive(Clone, Copy, Debug)]
enum Unmarked {
    Blind,
    LostClaim,
}

async fn unmarked(cause: Unmarked) {
    let Some(w) = strict_world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    script(
        &upstream,
        tool_turn("msg_u1", "toolu_extra2", "mcp__relay__agenda_extra", "{}"),
    );
    for n in 2..6 {
        script(&upstream, final_turn(&format!("msg_u{n}"), "All done"));
    }
    let run = w.accept("req-unmarked", "Organize my agenda").await;
    promote(&w).await;

    let tag = format!("{cause:?}");
    let judge: Arc<dyn Preflight> = match cause {
        Unmarked::Blind => blind_judge(&w),
        Unmarked::LostClaim => Arc::new(LosesTheClaim {
            inner: w.broker.clone(),
            pool: w.pg.store.pool().clone(),
        }),
    };
    let first = host_judged(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace(&format!("{tag}-1")),
        judge,
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("first run must complete");
    match cause {
        Unmarked::Blind => assert!(
            matches!(outcome, Some(Outcome::Failed(_))),
            "{cause:?}: unresolved call must retry, not succeed: {outcome:?}"
        ),
        Unmarked::LostClaim => assert_eq!(outcome, Some(Outcome::Lost), "{cause:?}"),
    }
    assert!(
        marks(&w, &run).await.is_empty(),
        "{cause:?}: tentative refusal must leave no marker"
    );
    assert!(w.runner.jobs().is_empty(), "{cause:?}");
    assert_ne!(w.pg.state_of(&run).await, "succeeded", "{cause:?}");
    let first_seen = bodies(&upstream);
    match cause {
        Unmarked::Blind => assert_word(&first_seen[1], "toolu_extra2", "unavailable"),
        Unmarked::LostClaim => {}
    }

    w.pg.sql(
        "update bot_runs set not_before_ms = 0, lease_until_ms = case when state = 'leased' then 0 else lease_until_ms end",
    )
    .await;
    drop(first);
    let second = host(
        &w,
        w.pg.peer("worker-2").await,
        upstream.clone(),
        sandbox,
        &workspace(&format!("{tag}-2")),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), second.worker.run_once())
        .await
        .expect("resumed run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded), "{cause:?}");
    assert!(w.runner.jobs().is_empty(), "{cause:?}");
    let seen = bodies(&upstream);
    assert_word(
        seen.last().expect("resumed request"),
        "toolu_extra2",
        "unknown_tool",
    );
}

#[tokio::test]
async fn a_verdict_that_could_not_confirm_the_pinned_revision_leaves_no_mark_and_the_resume_judges_again()
 {
    unmarked(Unmarked::Blind).await;
}

#[tokio::test]
async fn a_lost_claim_during_the_verdict_leaves_no_mark_and_the_new_owner_judges_again() {
    unmarked(Unmarked::LostClaim).await;
}

#[tokio::test]
async fn a_marked_refusal_lets_the_run_finish_in_one_attempt_without_running_anything() {
    let Some(w) = strict_world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    script(
        &upstream,
        tool_turn("msg_m1", "toolu_nope", "mcp__relay__agenda_extra", "{}"),
    );
    script(&upstream, final_turn("msg_m2", "That tool is unavailable"));
    let run = w.accept("req-marked", "Organize my agenda").await;
    promote(&w).await;
    let host = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox,
        &workspace("marked"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.pg.state_of(&run).await, "succeeded");
    let stored = marks(&w, &run).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].1, json_word(CliRefusal::UnknownTool));
    assert!(w.runner.jobs().is_empty());
    let seen = bodies(&upstream);
    assert_word(&seen[1], "toolu_nope", "unknown_tool");
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        r#"{"text":"That tool is unavailable"}"#
    );
}
