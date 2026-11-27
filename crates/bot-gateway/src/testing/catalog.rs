//! Mutable `ProfileCatalog` test double supporting hot swaps, mid-request flips, and revision history lookup.
use bot_core::{
    id::{ProfileId, Revision},
    profile::{ProfileCatalog, Snapshot},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub fn catalog(
    snapshots: impl IntoIterator<Item = Arc<Snapshot>>,
) -> Arc<BTreeMap<ProfileId, Arc<Snapshot>>> {
    Arc::new(snapshots.into_iter().map(|s| (s.id.clone(), s)).collect())
}

type ActiveSet = Arc<BTreeMap<ProfileId, Arc<Snapshot>>>;

pub struct TestCatalog {
    active: Mutex<Arc<BTreeMap<ProfileId, Arc<Snapshot>>>>,
    flip: Mutex<Option<(usize, ActiveSet)>>,
    history: Mutex<Vec<Arc<Snapshot>>>,
    strict: Mutex<bool>,
}

impl TestCatalog {
    pub fn new(snapshots: impl IntoIterator<Item = Arc<Snapshot>>) -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(catalog(snapshots)),
            flip: Mutex::new(None),
            history: Mutex::new(Vec::new()),
            strict: Mutex::new(false),
        })
    }

    pub fn remember(&self, snapshots: impl IntoIterator<Item = Arc<Snapshot>>) {
        self.history.lock().unwrap().extend(snapshots);
    }

    pub fn strict(&self, strict: bool) {
        *self.strict.lock().unwrap() = strict;
    }

    pub fn flip_after(&self, calls: usize, snapshots: impl IntoIterator<Item = Arc<Snapshot>>) {
        *self.flip.lock().unwrap() = Some((calls, catalog(snapshots)));
    }

    pub fn replace(&self, snapshots: impl IntoIterator<Item = Arc<Snapshot>>) {
        *self.active.lock().unwrap() = catalog(snapshots);
    }
}

impl ProfileCatalog for TestCatalog {
    fn active(&self) -> Arc<BTreeMap<ProfileId, Arc<Snapshot>>> {
        let mut flip = self.flip.lock().unwrap();
        if let Some((remaining, next)) = flip.as_mut() {
            if *remaining == 0 {
                *self.active.lock().unwrap() = next.clone();
                *flip = None;
            } else {
                *remaining -= 1;
            }
        }
        drop(flip);
        self.active.lock().unwrap().clone()
    }

    fn lookup(&self, profile: &ProfileId, revision: &Revision) -> Option<Arc<Snapshot>> {
        let remembered = self
            .history
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.id == *profile && s.revision == *revision)
            .cloned();
        if remembered.is_some() {
            return remembered;
        }
        let strict = *self.strict.lock().unwrap();
        self.active()
            .get(profile)
            .filter(|s| !strict || s.revision == *revision)
            .cloned()
    }
}
