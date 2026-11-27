//! Worker lifecycle tests: direct tool runs, cancellation, lease loss, concurrency, shutdown, gateway nudges, and spans.

use super::fakes::*;
use crate::{
    model::{ModelError, ModelRequest, ModelRunner, ModelStep, RunnerKind, StepFuture},
    worker::Outcome,
};
use bot_core::{command::*, id::*, port::Error, state::ConversationKind, trace::key};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;

struct Slow {
    ms: u64,
    now: AtomicUsize,
    peak: AtomicUsize,
}

impl Slow {
    fn new(ms: u64) -> Arc<Self> {
        Arc::new(Self {
            ms,
            now: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        })
    }
}

impl ModelRunner for Slow {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, _: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            let inflight = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(inflight, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(self.ms)).await;
            self.now.fetch_sub(1, Ordering::SeqCst);
            Ok(ModelStep::Final {
                text: "done".into(),
                usage: Default::default(),
                outcome: Default::default(),
            })
        })
    }
}

#[tokio::test]
async fn a_tool_target_runs_one_call_with_a_fixed_id_and_records_the_tool_result() {
    let r = rig(ScriptedModel::new(vec![]));
    r.broker.script(done(r#"{"events":[]}"#));
    assert_eq!(
        r.worker.execute(direct_run("r1", "agenda.list")).await,
        Outcome::Succeeded
    );
    let calls = r.broker.calls();
    assert_eq!(
        (calls[0].call.0.as_str(), calls[0].tool.0.as_str()),
        ("r1:tool", "agenda.list")
    );
    let state = lock(&r.runs.state);
    assert_eq!(
        state.finished[0].result,
        JsonPayload(r#"{"events":[]}"#.into())
    );
    assert!(
        state.finished[0].answers.is_empty(),
        "direct tool replies are routed by the tool destination"
    );
}

#[tokio::test]
async fn a_candidate_in_a_pass_chain_gets_its_own_call_id_and_a_pass_leaves_the_run_unfinished() {
    for (pos, id) in [(0, "r1:tool"), (1, "r1:tool:1"), (2, "r1:tool:2")] {
        let r = rig(ScriptedModel::new(vec![]));
        r.broker.script(Ok(CallResult::Passed));
        let mut ctx = direct_run("r1", "agenda.list");
        ctx.work.pass_pos = Some(pos);
        assert_eq!(r.worker.execute(ctx).await, Outcome::Parked);
        assert_eq!(r.broker.calls()[0].call.0, id, "candidate {pos}");
        let state = lock(&r.runs.state);
        assert!(
            state.finished.is_empty() && state.failed.is_empty(),
            "pass must leave completion to the store"
        );
    }
    let r = rig(ScriptedModel::new(vec![]));
    r.broker.script(done(r#"{"events":[]}"#));
    assert_eq!(
        r.worker.execute(direct_run("r1", "agenda.list")).await,
        Outcome::Succeeded
    );
    assert_eq!(r.broker.calls()[0].call.0, "r1:tool");
}

#[tokio::test]
async fn a_tool_target_failure_maps_to_a_fixed_code_and_an_approval_wait_parks() {
    for (error, expected) in [
        (Error::Denied, Failure::Permanent("denied")),
        (Error::Missing, Failure::Permanent("unknown_tool")),
        (Error::Invalid, Failure::Permanent("invalid_args")),
        (Error::Unavailable, Failure::Transient("broker_unavailable")),
    ] {
        let r = rig(ScriptedModel::new(vec![]));
        r.broker.script(Err(error));
        assert!(matches!(
            r.worker.execute(direct_run("r1", "agenda.list")).await,
            Outcome::Failed(_)
        ));
        assert_eq!(lock(&r.runs.state).failed, [expected]);
    }
    let r = rig(ScriptedModel::new(vec![]));
    r.broker
        .script(Ok(CallResult::Awaiting(ApprovalId("n1".into()))));
    assert_eq!(
        r.worker.execute(direct_run("r1", "agenda.delete")).await,
        Outcome::Parked
    );
}

#[tokio::test]
async fn a_cancel_request_seen_by_the_heartbeat_stops_the_model_and_ends_the_run_cancelled() {
    let r = rig(Arc::new(BlockingModel));
    r.runs.renew_script(Ok(Beat {
        cancel_requested: true,
    }));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Cancelled);
    assert_eq!(
        lock(&r.runs.state).failed,
        [Failure::Permanent("cancelled")]
    );
    assert!(lock(&r.runs.state).finished.is_empty());
}

#[tokio::test]
async fn a_cancel_that_lands_before_the_first_store_read_ends_the_run_cancelled_not_lost() {
    let model = ScriptedModel::new(vec![]);
    let r = rig(model.clone());
    lock(&r.runs.state).cancel = true;
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Cancelled);
    let state = lock(&r.runs.state);
    assert_eq!(state.failed, [Failure::Permanent("cancelled")]);
    assert!(state.finished.is_empty() && state.steps.is_empty());
    assert!(model.requests().is_empty(), "model must not be called");
}

#[tokio::test]
async fn a_stale_read_without_a_cancel_request_is_still_a_lost_claim() {
    let r = rig(Arc::new(BlockingModel));
    lock(&r.runs.state).history_error = Some(Error::Stale);
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Lost);
    let state = lock(&r.runs.state);
    assert!(state.finished.is_empty() && state.failed.is_empty() && state.steps.is_empty());
}

#[tokio::test]
async fn a_cancel_request_does_not_turn_a_really_lost_claim_into_a_cancel() {
    let r = rig(Arc::new(BlockingModel));
    lock(&r.runs.state).cancel = true;
    r.runs.renew_script(Err(Error::Stale));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Lost);
    let state = lock(&r.runs.state);
    assert!(state.finished.is_empty() && state.failed.is_empty() && state.steps.is_empty());
}

#[tokio::test]
async fn a_lease_the_heartbeat_already_lost_is_not_rechecked_into_a_cancel() {
    let r = rig(Arc::new(BlockingModel));
    r.runs.renew_script(Err(Error::Stale));
    r.runs.renew_script(Ok(Beat {
        cancel_requested: true,
    }));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Lost);
    let state = lock(&r.runs.state);
    assert!(state.finished.is_empty() && state.failed.is_empty() && state.steps.is_empty());
}

#[tokio::test]
async fn a_lost_lease_stops_the_model_and_writes_nothing() {
    let r = rig(Arc::new(BlockingModel));
    r.runs.renew_script(Err(Error::Stale));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Lost);
    let state = lock(&r.runs.state);
    assert!(state.finished.is_empty() && state.failed.is_empty() && state.steps.is_empty());
}

#[tokio::test]
async fn the_store_going_quiet_for_too_long_loses_the_lease_but_one_missed_beat_does_not() {
    let r = rig(Arc::new(BlockingModel));
    r.runs.renew_script(Err(Error::Unavailable));
    r.runs.renew_script(Err(Error::Unavailable));
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Lost);

    let r = rig(Slow::new(80));
    r.runs.renew_script(Err(Error::Unavailable));
    assert_eq!(r.worker.execute(agent_run("r2")).await, Outcome::Succeeded);
    assert!(
        lock(&r.runs.state).renews >= 3,
        "heartbeat should continue renewing while running"
    );
}

#[tokio::test]
async fn reporting_trouble_is_handled_without_overwriting_a_new_owner() {
    let r = rig(ScriptedModel::new(vec![say("done")]));
    lock(&r.runs.state).finish_error = Some(Error::Stale);
    assert_eq!(r.worker.execute(agent_run("r1")).await, Outcome::Lost);
    assert!(
        lock(&r.runs.state).failed.is_empty(),
        "lost owner must not report failure"
    );

    let r = rig(ScriptedModel::new(vec![say("done")]));
    lock(&r.runs.state).finish_error = Some(Error::Unavailable);
    assert!(matches!(
        r.worker.execute(agent_run("r2")).await,
        Outcome::Failed(_)
    ));
    assert_eq!(
        lock(&r.runs.state).failed,
        [Failure::Transient("finish_unavailable")]
    );
}

#[tokio::test]
async fn run_once_takes_one_run_or_nothing() {
    let r = rig(ScriptedModel::new(vec![say("done")]));
    assert_eq!(r.worker.run_once().await, None);
    r.runs.queue(agent_run("r1"));
    assert_eq!(r.worker.run_once().await, Some(Outcome::Succeeded));
    assert_eq!(r.worker.run_once().await, None);
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(started.elapsed() < Duration::from_secs(5), "{what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn the_loop_respects_the_concurrency_limit_and_stops_on_shutdown() {
    let model = Slow::new(40);
    let r = rig(model.clone());
    for n in 0..6 {
        r.runs.queue(agent_run(&format!("r{n}")));
    }
    let (stop, shutdown) = watch::channel(false);
    let worker = r.worker.clone();
    let serving = tokio::spawn(async move { worker.run(shutdown).await });
    until("all runs should finish", || {
        lock(&r.runs.state).finished.len() == 6
    })
    .await;
    assert_eq!(
        model.peak.load(Ordering::SeqCst),
        2,
        "concurrency must stay within limit (2)"
    );
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), serving)
        .await
        .expect("worker should exit promptly after shutdown")
        .unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_the_grace_period_then_lets_go_of_runs_in_flight() {
    let r = rig(Arc::new(BlockingModel));
    r.runs.queue(agent_run("r1"));
    let (stop, shutdown) = watch::channel(false);
    let worker = r.worker.clone();
    let serving = tokio::spawn(async move { worker.run(shutdown).await });
    until("run should start", || lock(&r.runs.state).renews >= 1).await;
    let started = Instant::now();
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), serving)
        .await
        .expect("worker should exit once grace elapses")
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "shutdown should wait through the grace period"
    );
    let state = lock(&r.runs.state);
    assert!(
        state.finished.is_empty() && state.failed.is_empty(),
        "dropped run should be left for lease handover"
    );
}

#[tokio::test]
async fn the_gateway_is_nudged_once_for_a_root_run_in_a_push_conversation_only() {
    let r = rig(ScriptedModel::new(vec![say("a"), say("b"), say("c")]));
    r.worker
        .execute(in_kind(agent_run("root-dm"), ConversationKind::Direct))
        .await;
    r.worker.execute(agent_run("root-cli")).await;
    let mut child = in_kind(agent_run("child"), ConversationKind::Direct);
    child.root = RunId("root-dm".into());
    r.worker.execute(child).await;
    assert_eq!(*lock(&r.gateway.delivered), [RunId("root-dm".into())]);
}

#[tokio::test]
async fn the_attempt_gets_a_root_span_with_outcome_and_model_child_spans_and_links_the_previous_generation()
 {
    let model = ScriptedModel::new(vec![
        calls("Checking now", &[("r1:0:0", "agenda.list", "{}")]),
        say("No events found."),
    ]);
    let r = rig(model);
    let run = generation(agent_run("r1"), 2);
    let prompt = match &run.work.target {
        Target::Agent { instruction, .. } => instruction.clone(),
        _ => unreachable!(),
    };
    assert_eq!(r.worker.execute(run).await, Outcome::Succeeded);
    let spans = lock(&r.spans.0).clone();
    let root = spans.iter().find(|s| s.name == "worker.run").unwrap();
    assert_eq!(root.str(key::BOT_OUTCOME), Some("succeeded"));
    assert_eq!(root.str(key::BOT_RUN_ID), Some("r1"));
    assert_eq!(root.int(key::BOT_ATTEMPT_GENERATION), Some(2));
    assert_eq!(root.links.len(), 1, "link to previous generation attempt");
    assert_eq!(root.error, None);
    let models: Vec<_> = spans.iter().filter(|s| s.name == "worker.model").collect();
    assert_eq!(models.len(), 2);
    assert!(
        models
            .iter()
            .all(|s| s.ids.parent == Some(root.ids.span) && s.ids.trace == root.ids.trace)
    );
    assert_eq!(models[1].int(key::GEN_AI_USAGE_INPUT_TOKENS), Some(10));
    for span in &spans {
        assert!(!format!("{span:?}").contains(&prompt));
    }

    let r = rig(Arc::new(BlockingModel));
    r.runs.renew_script(Err(Error::Stale));
    r.worker.execute(agent_run("r2")).await;
    let spans = lock(&r.spans.0).clone();
    let root = spans.iter().find(|s| s.name == "worker.run").unwrap();
    assert_eq!(root.str(key::BOT_OUTCOME), Some("lost"));
    assert!(root.links.is_empty(), "first generation has no link");

    let r = rig(ScriptedModel::new(vec![Err(ModelError::Timeout)]));
    r.worker.execute(agent_run("r3")).await;
    let spans = lock(&r.spans.0).clone();
    let root = spans.iter().find(|s| s.name == "worker.run").unwrap();
    assert_eq!(
        (root.str(key::BOT_OUTCOME), root.error),
        (Some("failed"), Some("timeout"))
    );
}

struct CountingRelays {
    opened: AtomicUsize,
    revoked: Arc<AtomicUsize>,
    seen: std::sync::Mutex<Vec<(String, String, String)>>,
}

struct Revoke(Arc<AtomicUsize>);

impl Drop for Revoke {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl crate::host::Relays for CountingRelays {
    fn open(
        &self,
        attempt: &bot_core::id::Attempt,
        agent: &AgentId,
        alias: &str,
    ) -> Option<crate::model::RelayGrant> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        lock(&self.seen).push((attempt.run.0.clone(), agent.0.clone(), alias.to_owned()));
        Some(crate::model::RelayGrant::new(
            crate::model::RelayChannel {
                name: "relay".into(),
                base_url: "http://192.168.64.1:8787".into(),
                token: format!("tok-{}", attempt.generation.0),
            },
            Revoke(self.revoked.clone()),
        ))
    }
}

struct Peek(Arc<std::sync::Mutex<Vec<Option<String>>>>);

impl ModelRunner for Peek {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        lock(&self.0).push(request.relay.as_ref().map(|r| r.token.clone()));
        Box::pin(async {
            Ok(ModelStep::Final {
                text: "reply".into(),
                usage: Default::default(),
                outcome: Default::default(),
            })
        })
    }
}

#[tokio::test]
async fn a_loop_run_opens_a_front_channel_for_its_own_attempt_and_revokes_only_that_grant_afterwards()
 {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let relays = Arc::new(CountingRelays {
        opened: AtomicUsize::new(0),
        revoked: Arc::new(AtomicUsize::new(0)),
        seen: Default::default(),
    });
    let r = rig_with_relays(Arc::new(Peek(seen.clone())), relays.clone());
    assert_eq!(
        r.worker.execute(generation(agent_run("r1"), 3)).await,
        Outcome::Succeeded
    );
    assert_eq!(
        *lock(&seen),
        [Some("tok-3".to_owned())],
        "attempt-scoped token must reach the runner"
    );
    assert_eq!(
        *lock(&relays.seen),
        [("r1".to_owned(), "main".to_owned(), "assistant".to_owned())]
    );
    assert_eq!(relays.opened.load(Ordering::SeqCst), 1);
    assert_eq!(
        relays.revoked.load(Ordering::SeqCst),
        1,
        "grant must be revoked after completion"
    );

    let relays = Arc::new(CountingRelays {
        opened: AtomicUsize::new(0),
        revoked: Arc::new(AtomicUsize::new(0)),
        seen: Default::default(),
    });
    let r = rig_with_relays(Arc::new(BlockingModel), relays.clone());
    r.runs.renew_script(Err(Error::Stale));
    assert_eq!(r.worker.execute(agent_run("r2")).await, Outcome::Lost);
    assert_eq!(relays.revoked.load(Ordering::SeqCst), 1);

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let r = rig(Arc::new(Peek(seen.clone())));
    r.worker.execute(agent_run("r3")).await;
    assert_eq!(*lock(&seen), [None]);
}

#[tokio::test]
async fn a_cancel_at_finish_is_reported_without_nudging_the_gateway() {
    let r = rig(ScriptedModel::new(vec![]));
    lock(&r.runs.state).cancel = true;
    let run = in_kind(
        direct_run("root-dm", "agenda.list"),
        ConversationKind::Direct,
    );
    assert_eq!(r.worker.execute(run).await, Outcome::Cancelled);
    assert!(lock(&r.gateway.delivered).is_empty());
    assert!(lock(&r.runs.state).failed.is_empty());
}
