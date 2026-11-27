//! Deterministic `step` read hooks for pausing or triggering concurrent events mid-read in tests.

use super::Hook;
use bot_core::{
    command::JsonPayload,
    port::{Error, Reply},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

#[derive(Default)]
pub(super) struct StepProbe {
    hold: Mutex<Option<(String, Arc<Semaphore>)>>,
    after: Mutex<Option<(String, Hook)>>,
    held: Notify,
}

#[derive(Default)]
pub(super) struct Taken {
    hold: Option<Arc<Semaphore>>,
    after: Option<Hook>,
}

use super::super::lock;

/// Consumes a one-shot hook when `key` starts with its configured prefix.
fn take_if<T>(slot: &Mutex<Option<(String, T)>>, key: &str) -> Option<T> {
    let mut slot = lock(slot);
    match slot.take() {
        Some((prefix, value)) if key.starts_with(&prefix) => Some(value),
        other => {
            *slot = other;
            None
        }
    }
}

impl StepProbe {
    pub(super) fn hold(&self, prefix: &str) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *lock(&self.hold) = Some((prefix.to_owned(), gate.clone()));
        gate
    }

    pub(super) fn after(&self, prefix: &str, hook: Hook) {
        *lock(&self.after) = Some((prefix.to_owned(), hook));
    }

    pub(super) async fn entered(&self) {
        self.held.notified().await;
    }

    pub(super) fn take(&self, key: &str) -> Taken {
        Taken {
            hold: take_if(&self.hold, key),
            after: take_if(&self.after, key),
        }
    }

    pub(super) fn wrap<'a>(
        &'a self,
        taken: Taken,
        delay: Duration,
        reply: Reply<'a, Option<JsonPayload>>,
    ) -> Reply<'a, Option<JsonPayload>> {
        if delay.is_zero() && taken.hold.is_none() && taken.after.is_none() {
            return reply;
        }
        Box::pin(async move {
            if let Some(gate) = taken.hold {
                self.held.notify_one();
                let _ = gate.acquire().await;
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let out: Result<Option<JsonPayload>, Error> = reply.await;
            if let Some(after) = taken.after {
                after();
            }
            out
        })
    }
}
