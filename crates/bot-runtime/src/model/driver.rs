//! Single CLI model invocation driver enforcing total/idle timeouts, cancellation, and fixed-code error classification.

use super::{Limits, ModelError, Usage};
use bot_core::{
    command::RunOutcome,
    sandbox::{Exit, ModelProcess, ModelSandbox, ModelSpec},
};
use tokio::{sync::watch, time::Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Thinking,
    Writing,
    Retrying,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Output {
    pub text: String,
    pub usage: Usage,
    pub outcome: RunOutcome,
}

pub trait Parser: Send {
    fn feed(&mut self, line: &str) -> Option<Event>;
    fn has_result(&self) -> bool;
    fn finish(self: Box<Self>) -> Result<Output, ModelError>;
}

pub fn is_transient(message: &str) -> bool {
    classify(message, None).1
}

/// Maps raw error text and HTTP status to a fixed `(code, transient)` pair without returning raw text.
pub fn classify(message: &str, status: Option<u64>) -> (&'static str, bool) {
    let m = message.to_lowercase();
    if m.contains("output token maximum") {
        return ("output_limit", false);
    }
    if status == Some(429) || m.contains("429") || m.contains("rate limit") {
        return ("rate_limited", true);
    }
    let overloaded = matches!(status, Some(500 | 502 | 503 | 504 | 529))
        || ["503", "502", "504", "529", "overloaded", "temporarily"]
            .iter()
            .any(|k| m.contains(k));
    if overloaded {
        return ("overloaded", true);
    }
    if ["timed out", "econnreset", "socket hang up"]
        .iter()
        .any(|k| m.contains(k))
    {
        return ("network", true);
    }
    ("model_error", false)
}

/// Resolves when `rx` becomes `true`; stays pending forever if the sender is dropped.
pub(super) async fn cancelled(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn halt(process: &mut Box<dyn ModelProcess>, error: ModelError) -> ModelError {
    process.kill().await;
    error
}

pub async fn drive(
    sandbox: &ModelSandbox,
    spec: ModelSpec,
    parser: Box<dyn Parser>,
    limits: Limits,
    cancel: watch::Receiver<bool>,
) -> Result<Output, ModelError> {
    drive_with(
        sandbox,
        spec,
        parser,
        limits,
        cancel,
        watch::channel(false).1,
    )
    .await
}

/// Runs a model process like `drive`, also honoring a host-side `abort` signal (e.g. session integrity failure).
pub async fn drive_with(
    sandbox: &ModelSandbox,
    spec: ModelSpec,
    mut parser: Box<dyn Parser>,
    limits: Limits,
    mut cancel: watch::Receiver<bool>,
    mut abort: watch::Receiver<bool>,
) -> Result<Output, ModelError> {
    if *cancel.borrow() {
        return Err(ModelError::Cancelled);
    }
    // Enforce cancel/abort during spawn as well (`Sandbox::spawn` leaves no orphan VM if dropped).
    let mut process = tokio::select! {
        biased;
        () = cancelled(&mut cancel) => return Err(ModelError::Cancelled),
        () = cancelled(&mut abort) => return Err(ModelError::Cancelled),
        spawned = sandbox.spawn(spec) => spawned.map_err(|_| ModelError::Failed {
            code: "sandbox_failed",
            transient: true,
        })?,
    };
    let deadline = Instant::now() + limits.total;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(halt(&mut process, ModelError::Timeout).await);
        }
        let wait = limits.idle.min(remaining);
        tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => return Err(halt(&mut process, ModelError::Cancelled).await),
            _ = cancelled(&mut abort) => return Err(halt(&mut process, ModelError::Cancelled).await),
            line = tokio::time::timeout(wait, process.next_line()) => match line {
                Ok(Some(line)) => {
                    parser.feed(&line);
                }
                Ok(None) => break,
                Err(_) => {
                    let error = if Instant::now() >= deadline {
                        ModelError::Timeout
                    } else {
                        ModelError::Failed { code: "idle_timeout", transient: true }
                    };
                    return Err(halt(&mut process, error).await);
                }
            },
        }
    }
    // Enforce remaining total deadline and cancel even after stdout closes while waiting for exit.
    let remaining = deadline.saturating_duration_since(Instant::now());
    let exit: Exit = tokio::select! {
        biased;
        _ = cancelled(&mut cancel) => return Err(halt(&mut process, ModelError::Cancelled).await),
        _ = cancelled(&mut abort) => return Err(halt(&mut process, ModelError::Cancelled).await),
        exit = tokio::time::timeout(remaining, process.wait()) => match exit {
            Ok(exit) => exit,
            Err(_) => return Err(halt(&mut process, ModelError::Timeout).await),
        },
    };
    if parser.has_result() {
        return parser.finish();
    }
    // Without a result event, classify stderr only to distinguish transient faults from permanent crashes.
    Err(match classify(&exit.stderr_tail, None) {
        (code, true) => ModelError::Failed {
            code,
            transient: true,
        },
        _ => ModelError::Failed {
            code: "no_result",
            transient: false,
        },
    })
}

#[cfg(test)]
mod tests;
