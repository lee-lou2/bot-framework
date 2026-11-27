//! CLI driver tests: clean exit, stderr error classification, idle/total timeouts, and cancellation during spawn.

use super::*;
use crate::model::{FakeSandbox, Script};
use bot_core::{
    id::{Attempt, ClaimEpoch, Generation, ProfileId, RunId},
    port::Reply,
    sandbox::{ModelProcess, ModelSpec, Sandbox},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Collect {
    lines: Vec<String>,
    done: bool,
}

impl Parser for Collect {
    fn feed(&mut self, line: &str) -> Option<Event> {
        self.done |= line == "DONE";
        self.lines.push(line.to_owned());
        None
    }

    fn has_result(&self) -> bool {
        self.done
    }

    fn finish(self: Box<Self>) -> Result<Output, ModelError> {
        Ok(Output {
            text: self.lines.join("|"),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
            },
            outcome: Default::default(),
        })
    }
}

fn spec() -> ModelSpec {
    ModelSpec {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            generation: Generation(1),
            claim: Some(ClaimEpoch(1)),
        },
        program: "cli".into(),
        args: vec![],
        env: vec![],
        cwd: "/bot".into(),
        readonly_mounts: vec![],
        scratch: None,
        stdin: None,
        timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
    }
}

fn limits(total_ms: u64, idle_ms: u64) -> Limits {
    Limits {
        total: Duration::from_millis(total_ms),
        idle: Duration::from_millis(idle_ms),
    }
}

async fn run(
    sandbox: &FakeSandbox,
    limits: Limits,
    cancel: watch::Receiver<bool>,
) -> Result<Output, ModelError> {
    drive(sandbox, spec(), Box::<Collect>::default(), limits, cancel).await
}

#[tokio::test]
async fn returns_the_parsers_result_when_the_process_finishes() {
    let sandbox = FakeSandbox::new([Script::output("a\nb\nDONE")]);
    let (_tx, rx) = watch::channel(false);
    let out = run(&sandbox, limits(2000, 2000), rx).await.unwrap();
    assert_eq!(
        (out.text.as_str(), out.usage.output_tokens),
        ("a|b|DONE", 2)
    );
    assert_eq!(sandbox.killed(), 0, "clean exit must not kill process");
}

#[tokio::test]
async fn a_run_without_a_result_is_classified_from_stderr_only() {
    let (_tx, rx) = watch::channel(false);
    for (stderr, want) in [
        (
            "503 Service Unavailable",
            ModelError::Failed {
                code: "overloaded",
                transient: true,
            },
        ),
        (
            "429 Too Many Requests",
            ModelError::Failed {
                code: "rate_limited",
                transient: true,
            },
        ),
        (
            "socket hang up",
            ModelError::Failed {
                code: "network",
                transient: true,
            },
        ),
        (
            "Segmentation fault",
            ModelError::Failed {
                code: "no_result",
                transient: false,
            },
        ),
        (
            "",
            ModelError::Failed {
                code: "no_result",
                transient: false,
            },
        ),
    ] {
        let sandbox = FakeSandbox::new([Script::output("partial").exits(1, stderr)]);
        assert_eq!(
            run(&sandbox, limits(2000, 2000), rx.clone()).await,
            Err(want),
            "{stderr}"
        );
    }
}

#[tokio::test]
async fn silence_past_the_idle_limit_is_a_transient_failure_and_kills_the_process() {
    let sandbox = FakeSandbox::new([Script::output("first").hangs()]);
    let (_tx, rx) = watch::channel(false);
    let error = run(&sandbox, limits(5000, 60), rx).await.unwrap_err();
    assert_eq!(
        error,
        ModelError::Failed {
            code: "idle_timeout",
            transient: true
        }
    );
    assert_eq!(sandbox.killed(), 1);
}

#[tokio::test]
async fn the_total_limit_stops_a_chatty_process() {
    let sandbox =
        FakeSandbox::new([Script::output(&"x\n".repeat(100)).slow(Duration::from_millis(30))]);
    let (_tx, rx) = watch::channel(false);
    let error = run(&sandbox, limits(150, 5000), rx).await.unwrap_err();
    assert_eq!(
        error,
        ModelError::Timeout,
        "total limit must still fire despite continuous output"
    );
    assert!(error.transient());
    assert_eq!(sandbox.killed(), 1);
}

#[tokio::test]
async fn cancel_kills_the_process_and_reports_cancelled() {
    let sandbox = FakeSandbox::new([Script::output("first").hangs()]);
    let (tx, rx) = watch::channel(false);
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true).unwrap();
        tx
    });
    let error = run(&sandbox, limits(5000, 5000), rx).await.unwrap_err();
    assert_eq!(error, ModelError::Cancelled);
    assert!(!error.transient());
    assert_eq!(
        error.failure(),
        bot_core::command::Failure::Permanent("cancelled")
    );
    assert_eq!(sandbox.killed(), 1);
    canceller.await.unwrap();
}

#[tokio::test]
async fn a_cancel_that_is_already_set_never_starts_the_sandbox() {
    let sandbox = FakeSandbox::new([Script::output("DONE")]);
    let (_tx, rx) = watch::channel(true);
    assert_eq!(
        run(&sandbox, limits(2000, 2000), rx).await,
        Err(ModelError::Cancelled)
    );
    assert!(sandbox.specs().is_empty());
}

#[tokio::test]
async fn a_dropped_cancel_sender_is_not_a_cancel() {
    let sandbox = FakeSandbox::new([Script::output("a\nDONE")]);
    let (tx, rx) = watch::channel(false);
    drop(tx);
    assert_eq!(
        run(&sandbox, limits(2000, 2000), rx).await.unwrap().text,
        "a|DONE"
    );
}

#[tokio::test]
async fn a_refused_sandbox_fails_closed_without_a_fallback() {
    let sandbox = FakeSandbox::new([Script::refused("no_runtime")]);
    let (_tx, rx) = watch::channel(false);
    assert_eq!(
        run(&sandbox, limits(2000, 2000), rx).await,
        Err(ModelError::Failed {
            code: "sandbox_failed",
            transient: true
        })
    );
}

#[test]
fn classify_maps_statuses_and_phrases_to_fixed_codes() {
    for (message, status, want) in [
        ("rate limit exceeded", None, ("rate_limited", true)),
        ("anything", Some(429), ("rate_limited", true)),
        (
            "API Error: 503 Service temporarily unavailable",
            None,
            ("overloaded", true),
        ),
        ("x", Some(529), ("overloaded", true)),
        ("x", Some(500), ("overloaded", true)),
        ("Request timed out", None, ("network", true)),
        ("read ECONNRESET", None, ("network", true)),
        ("Invalid API key", Some(401), ("model_error", false)),
        ("", None, ("model_error", false)),
        (
            "API Error: Claude's response exceeded the 128000 output token maximum. To configure this behavior, set the CLAUDE_CODE_MAX_OUTPUT_TOKENS environment variable.",
            None,
            ("output_limit", false),
        ),
    ] {
        assert_eq!(classify(message, status), want, "{message} {status:?}");
    }
    assert!(is_transient("503") && !is_transient("401 Unauthorized"));
}

struct Slow {
    delay: Duration,
    inner: FakeSandbox,
    started: Arc<AtomicUsize>,
    abandoned: Arc<AtomicUsize>,
}

impl Slow {
    fn new(delay: Duration, scripts: impl IntoIterator<Item = Script>) -> Self {
        Self {
            delay,
            inner: FakeSandbox::new(scripts),
            started: Arc::default(),
            abandoned: Arc::default(),
        }
    }
}

struct Pending(Option<Arc<AtomicUsize>>);

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(counter) = self.0.take() {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Sandbox<ModelSpec> for Slow {
    type Process = Box<dyn ModelProcess>;

    fn spawn(&self, spec: ModelSpec) -> Reply<'_, Self::Process> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            let mut pending = Pending(Some(self.abandoned.clone()));
            tokio::time::sleep(self.delay).await;
            pending.0 = None;
            self.inner.spawn(spec).await
        })
    }

    fn reap(&self, owner: &str) -> Reply<'_, ()> {
        self.inner.reap(owner)
    }
}

#[tokio::test]
async fn a_cancel_during_a_slow_spawn_returns_at_once_and_drops_the_start() {
    let sandbox = Slow::new(Duration::from_secs(30), [Script::output("DONE")]);
    let (tx, rx) = watch::channel(false);
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true).unwrap();
        tx
    });
    let began = std::time::Instant::now();
    let error = run_slow(&sandbox, rx).await.unwrap_err();
    assert_eq!(error, ModelError::Cancelled);
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(sandbox.started.load(Ordering::SeqCst), 1);
    assert_eq!(
        sandbox.abandoned.load(Ordering::SeqCst),
        1,
        "cancelled spawn must be dropped before launching"
    );
    assert!(
        sandbox.inner.specs().is_empty(),
        "no process should be spawned after cancel"
    );
    canceller.await.unwrap();
}

#[tokio::test]
async fn an_abort_during_a_slow_spawn_also_returns_at_once() {
    let sandbox = Slow::new(Duration::from_secs(30), [Script::output("DONE")]);
    let (_cancel_tx, cancel) = watch::channel(false);
    let (abort_tx, abort) = watch::channel(false);
    let aborter = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        abort_tx.send(true).unwrap();
        abort_tx
    });
    let began = std::time::Instant::now();
    let error = drive_with(
        &sandbox,
        spec(),
        Box::<Collect>::default(),
        limits(5000, 5000),
        cancel,
        abort,
    )
    .await
    .unwrap_err();
    assert_eq!(error, ModelError::Cancelled);
    assert!(began.elapsed() < Duration::from_secs(2));
    assert_eq!(sandbox.abandoned.load(Ordering::SeqCst), 1);
    aborter.await.unwrap();
}

#[tokio::test]
async fn a_spawn_that_finishes_in_time_is_used_and_a_later_cancel_still_kills_it() {
    let sandbox = Slow::new(Duration::from_millis(30), [Script::output("first").hangs()]);
    let (tx, rx) = watch::channel(false);
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(true).unwrap();
        tx
    });
    assert_eq!(
        run_slow(&sandbox, rx).await.unwrap_err(),
        ModelError::Cancelled
    );
    assert_eq!(sandbox.abandoned.load(Ordering::SeqCst), 0);
    assert_eq!(
        sandbox.inner.killed(),
        1,
        "spawned process must be killed on cancel"
    );
    canceller.await.unwrap();
}

#[tokio::test]
async fn a_failed_spawn_is_still_a_closed_failure_not_a_cancel() {
    let sandbox = Slow::new(Duration::from_millis(10), [Script::refused("no_runtime")]);
    let (_tx, rx) = watch::channel(false);
    assert_eq!(
        run_slow(&sandbox, rx).await.unwrap_err(),
        ModelError::Failed {
            code: "sandbox_failed",
            transient: true
        }
    );
}

async fn run_slow(sandbox: &Slow, cancel: watch::Receiver<bool>) -> Result<Output, ModelError> {
    drive(
        sandbox,
        spec(),
        Box::<Collect>::default(),
        limits(5000, 5000),
        cancel,
    )
    .await
}
