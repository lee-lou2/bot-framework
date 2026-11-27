//! Checks whether this process's profile set matches the store's active revision.

use crate::{
    id::Revision,
    port::{Error, Reply},
    profile::ActiveRevisions,
};
use std::sync::{Arc, RwLock};

pub trait ActiveCheck: Send + Sync {
    /// Returns `Stale` on mismatch or before publication so outdated processes stop opening new work; `Unavailable` on store error.
    fn verify(&self) -> Reply<'_, ()>;
}

pub struct ActiveSet {
    store: Arc<dyn ActiveRevisions>,
    expected: Arc<RwLock<Revision>>,
}

impl ActiveSet {
    pub fn new(store: Arc<dyn ActiveRevisions>, expected: Revision) -> Self {
        Self::live(store, Arc::new(RwLock::new(expected)))
    }

    /// Tracks a shared revision updated in place by live skill/MCP activation.
    pub fn live(store: Arc<dyn ActiveRevisions>, expected: Arc<RwLock<Revision>>) -> Self {
        Self { store, expected }
    }
}

impl ActiveCheck for ActiveSet {
    fn verify(&self) -> Reply<'_, ()> {
        Box::pin(async move {
            let expected = self
                .expected
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            match self.store.current().await? {
                Some(current) if current == expected => Ok(()),
                _ => Err(Error::Stale),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        command::{MemorySeed, TriggerSpec},
        grant::{GrantId, GrantPlan, GrantSpec},
        id::ProfileId,
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::Mutex,
        task::{Context, Poll, Waker},
    };

    struct FakeActive(Mutex<Option<Revision>>);

    impl ActiveRevisions for FakeActive {
        fn current(&self) -> Reply<'_, Option<Revision>> {
            let current = self.0.lock().unwrap().clone();
            Box::pin(std::future::ready(Ok(current)))
        }

        fn replace(
            &self,
            _: Option<Revision>,
            _: Revision,
            _: BTreeMap<ProfileId, Revision>,
            _: Vec<MemorySeed>,
            _: BTreeMap<ProfileId, Vec<TriggerSpec>>,
        ) -> Reply<'_, ()> {
            Box::pin(std::future::ready(Err(Error::Unavailable)))
        }

        fn replace_prepared(
            &self,
            _: Option<Revision>,
            _: Revision,
            _: BTreeMap<ProfileId, Revision>,
            _: Vec<MemorySeed>,
            _: BTreeMap<ProfileId, Vec<TriggerSpec>>,
            _: BTreeSet<(ProfileId, Revision)>,
        ) -> Reply<'_, ()> {
            Box::pin(std::future::ready(Err(Error::Unavailable)))
        }

        fn replace_granted(
            &self,
            _: Option<Revision>,
            _: Revision,
            _: BTreeMap<ProfileId, Revision>,
            _: Vec<MemorySeed>,
            _: BTreeMap<ProfileId, Vec<TriggerSpec>>,
            _: GrantPlan,
        ) -> Reply<'_, Vec<(GrantId, GrantSpec)>> {
            Box::pin(std::future::ready(Err(Error::Unavailable)))
        }

        fn replace_live(
            &self,
            _: Option<Revision>,
            _: Revision,
            _: BTreeMap<ProfileId, Revision>,
            _: Vec<MemorySeed>,
            _: BTreeMap<ProfileId, Vec<TriggerSpec>>,
            _: BTreeSet<(ProfileId, Revision)>,
        ) -> Reply<'_, ()> {
            Box::pin(std::future::ready(Err(Error::Unavailable)))
        }
    }

    fn poll_now<T>(mut fut: Reply<'_, T>) -> Result<T, Error> {
        let mut cx = Context::from_waker(Waker::noop());
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(out) => out,
            Poll::Pending => panic!("ready future expected"),
        }
    }

    #[test]
    fn live_active_set_follows_shared_revision_updates() {
        let store = Arc::new(FakeActive(Mutex::new(Some(Revision("r1".into())))));
        let shared = Arc::new(RwLock::new(Revision("r1".into())));
        let check = ActiveSet::live(store.clone(), shared.clone());
        assert!(poll_now(check.verify()).is_ok());

        *store.0.lock().unwrap() = Some(Revision("r2".into()));
        assert!(matches!(poll_now(check.verify()), Err(Error::Stale)));

        *shared.write().unwrap() = Revision("r2".into());
        assert!(poll_now(check.verify()).is_ok());
    }
}
