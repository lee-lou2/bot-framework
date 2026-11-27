//! Activation-revision guards that fence stale workers from claiming runs or calling tools.
use bot_core::{
    active::ActiveCheck,
    command::{
        Accept, AcceptExtras, Beat, CallResult, Completion, Failure, Finish, Retry, RunOutcome,
        Step, ToolCall, Turn,
    },
    id::{ApprovalId, Attempt, PrincipalId, ProfileId, RunId},
    intake::{RequestKey, RequestState, RunProgress},
    port::{Broker, Error, ModelCall, Reply, RunStore},
    state::RunContext,
};
use bot_store::Store;
use std::{sync::Arc, time::Duration};

/// Store port allowing a stale worker to return a claimed run to the queue without spending a retry attempt.
pub trait Handover: Send + Sync {
    /// Atomically decides in PostgreSQL under the active-row lock whether to release (stale owner) or record a normal failure.
    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry>;

    /// Conditionally releases a run back to the queue only if this handle is stale and still owns the lease/generation.
    fn release(&self, attempt: Attempt) -> Reply<'_, ()>;
}

impl Handover for Store {
    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry> {
        Box::pin(async move { self.fail_or_release(&attempt, failure).await })
    }

    fn release(&self, attempt: Attempt) -> Reply<'_, ()> {
        Box::pin(async move { self.release_stale(&attempt).await })
    }
}

pub struct StoreHandback(pub Arc<dyn Handover>);

impl bot_runtime::Handback for StoreHandback {
    fn release(&self, attempt: Attempt) -> Reply<'_, ()> {
        self.0.release(attempt)
    }
}

const FAIL_TRIES: usize = 4;
const FAIL_PAUSE: Duration = Duration::from_millis(250);

pub struct GuardedRuns {
    inner: Arc<dyn RunStore>,
    active: Arc<dyn ActiveCheck>,
    handover: Option<Arc<dyn Handover>>,
    tries: usize,
    pause: Duration,
}

impl GuardedRuns {
    pub fn new(inner: Arc<dyn RunStore>, active: Arc<dyn ActiveCheck>) -> Self {
        Self {
            inner,
            active,
            handover: None,
            tries: FAIL_TRIES,
            pause: FAIL_PAUSE,
        }
    }

    pub fn with_handover(mut self, handover: Arc<dyn Handover>) -> Self {
        self.handover = Some(handover);
        self
    }

    #[cfg(test)]
    pub fn with_retry(mut self, tries: usize, pause: Duration) -> Self {
        (self.tries, self.pause) = (tries.max(1), pause);
        self
    }
}

impl RunStore for GuardedRuns {
    fn accept_with(&self, command: Accept, extras: AcceptExtras) -> Reply<'_, RunId> {
        self.inner.accept_with(command, extras)
    }

    fn status(&self, key: RequestKey) -> Reply<'_, Option<RequestState>> {
        self.inner.status(key)
    }

    fn progress(&self, profile: ProfileId, run: RunId) -> Reply<'_, Option<RunProgress>> {
        self.inner.progress(profile, run)
    }

    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        Box::pin(async move {
            if self.active.verify().await.is_err() {
                tracing::warn!(
                    code = "stale_worker",
                    "active revision changed; refusing to claim new runs — restart with the new revision"
                );
                return Ok(None);
            }
            self.inner.claim().await
        })
    }

    // Shutdown release bypasses staleness checks so a stale worker still hands back its claim promptly.
    fn release_for_shutdown(&self, attempt: Attempt) -> Reply<'_, ()> {
        self.inner.release_for_shutdown(attempt)
    }

    fn renew(&self, attempt: Attempt) -> Reply<'_, Beat> {
        self.inner.renew(attempt)
    }

    fn checkpoint_batch(&self, attempt: Attempt, steps: Vec<Step>) -> Reply<'_, ()> {
        self.inner.checkpoint_batch(attempt, steps)
    }

    fn finish(&self, attempt: Attempt, done: Finish) -> Reply<'_, Completion> {
        self.inner.finish(attempt, done)
    }

    fn finish_with(
        &self,
        attempt: Attempt,
        done: Finish,
        outcome: RunOutcome,
    ) -> Reply<'_, Completion> {
        self.inner.finish_with(attempt, done, outcome)
    }

    // Retries `Unavailable` rather than recording a normal failure so a stale worker never dead-letters a run on lock timeout.
    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry> {
        Box::pin(async move {
            let Some(handover) = &self.handover else {
                return self.inner.fail(attempt, failure).await;
            };
            let mut pause = self.pause;
            for left in (0..self.tries).rev() {
                match handover.fail(attempt.clone(), failure).await {
                    Ok(Retry::After(delay)) if delay.is_zero() => {
                        tracing::warn!(
                            code = "stale_worker",
                            "stale worker returned run to queue for a current worker to pick up"
                        );
                        return Ok(Retry::After(delay));
                    }
                    Err(Error::Unavailable) if left > 0 => {
                        tokio::time::sleep(pause).await;
                        pause += self.pause;
                    }
                    other => return other,
                }
            }
            Err(Error::Unavailable)
        })
    }

    fn history(&self, attempt: Attempt, limit: u32) -> Reply<'_, Vec<Turn>> {
        self.inner.history(attempt, limit)
    }
}

pub struct GuardedBroker {
    inner: Arc<dyn Broker>,
    active: Arc<dyn ActiveCheck>,
}

impl GuardedBroker {
    pub fn new(inner: Arc<dyn Broker>, active: Arc<dyn ActiveCheck>) -> Self {
        Self { inner, active }
    }
}

impl Broker for GuardedBroker {
    fn call(&self, call: ToolCall) -> Reply<'_, CallResult> {
        Box::pin(async move {
            self.active.verify().await?;
            self.inner.call(call).await
        })
    }

    fn call_for_model(&self, call: ToolCall) -> Reply<'_, ModelCall> {
        Box::pin(async move {
            self.active.verify().await?;
            self.inner.call_for_model(call).await
        })
    }

    fn decide(&self, nonce: ApprovalId, actor: PrincipalId, allow: bool) -> Reply<'_, ()> {
        self.inner.decide(nonce, actor, allow)
    }
}

mod upstream;
mod wire;

pub use upstream::GuardedUpstream;
pub use wire::GuardedWire;

#[cfg(test)]
mod tests;
