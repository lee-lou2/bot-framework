//! Per-call-slot async serialization for concurrent `tools/call` redeliveries.
//!
//! Serializes mark-read -> broker call -> mark-write per `(profile, run, call)` within a claim so
//! concurrent retries cannot race into both a refusal marker and a live execution. Cross-claim
//! races are fenced by the store's claim check.

use bot_core::id::{CallId, ProfileId, RunId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{Mutex as Door, OwnedMutexGuard};

pub(super) type Key = (ProfileId, RunId, CallId);

#[derive(Default)]
pub(super) struct Slots {
    open: Mutex<HashMap<Key, Arc<Door<()>>>>,
}

pub(super) struct Entered<'a> {
    slots: &'a Slots,
    key: Key,
    door: Option<Arc<Door<()>>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl Slots {
    pub(super) async fn enter(&self, key: Key) -> Entered<'_> {
        let door = self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default()
            .clone();
        // Construct `Entered` before awaiting so cancellation still prunes idle entries in `Drop`.
        let mut entered = Entered {
            slots: self,
            key,
            door: Some(door.clone()),
            guard: None,
        };
        entered.guard = Some(door.lock_owned().await);
        entered
    }

    #[cfg(test)]
    pub(super) fn open(&self) -> usize {
        self.open.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Reference counts: 1 map entry + 2 for the holder (`Entered` + guard) + 2 per waiter.
    #[cfg(test)]
    pub(super) fn waiting(&self, key: &Key) -> usize {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map_or(0, |door| Arc::strong_count(door).saturating_sub(3) / 2)
    }
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        // Release the mutex and local `Arc` first, then remove the map entry under the map lock only if unshared.
        self.guard.take();
        self.door.take();
        let mut open = self.slots.open.lock().unwrap_or_else(|e| e.into_inner());
        if open
            .get(&self.key)
            .is_some_and(|door| Arc::strong_count(door) == 1)
        {
            open.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::{sync::mpsc, time::timeout};

    fn key(call: &str) -> Key {
        (
            ProfileId("personal".into()),
            RunId("r1".into()),
            CallId(call.into()),
        )
    }

    #[tokio::test]
    async fn the_same_position_goes_one_at_a_time_and_other_positions_do_not_wait() {
        let slots = Arc::new(Slots::default());
        let first = slots.enter(key("cli-1-0")).await;
        drop(slots.enter(key("cli-1-1")).await);
        let (entered, mut seen) = mpsc::unbounded_channel();
        let second = {
            let slots = slots.clone();
            tokio::spawn(async move {
                let _in = slots.enter(key("cli-1-0")).await;
                let _ = entered.send(());
            })
        };
        assert!(
            timeout(Duration::from_millis(80), seen.recv())
                .await
                .is_err(),
            "second request for the same slot must wait while the first holds it"
        );
        drop(first);
        timeout(Duration::from_secs(5), seen.recv())
            .await
            .expect("second request enters after release")
            .expect("signal");
        second.await.unwrap();
        assert_eq!(slots.open(), 0, "idle slot must be removed from map");
    }

    #[tokio::test]
    async fn a_waiter_that_gives_up_leaves_nothing_behind() {
        let slots = Arc::new(Slots::default());
        let first = slots.enter(key("cli-2-0")).await;
        let giving_up = timeout(Duration::from_millis(40), slots.enter(key("cli-2-0"))).await;
        assert!(giving_up.is_err(), "waiter timed out");
        assert_eq!(slots.open(), 1, "only holder remains");
        drop(first);
        assert_eq!(slots.open(), 0);
        drop(slots.enter(key("cli-2-0")).await);
        assert_eq!(slots.open(), 0);
    }
}
