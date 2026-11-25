//! 활성 프로필 집합을 시험이 통째로 바꾸고 지난 판본을 복원해 주는 `ProfileCatalog`. `catalog()`로 합성 집합을 만든다.
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

/// 활성 집합을 시험이 통째로 바꿀 수 있는 ProfileCatalog
pub struct TestCatalog {
    active: Mutex<Arc<BTreeMap<ProfileId, Arc<Snapshot>>>>,
    /// `active()`를 n번 부른 뒤 이 집합으로 바꾼다(요청 처리 도중 교체 흉내)
    flip: Mutex<Option<(usize, ActiveSet)>>,
    /// 지난 판본(고정한 판본을 `lookup`으로 복원하는 시험용)
    history: Mutex<Vec<Arc<Snapshot>>>,
    /// 참이면 `lookup`이 활성이거나 기억한 판본만 돌려준다(아니면 판본과 무관하게 활성 스냅숏을 돌려준다)
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

    /// 지난 판본을 기억해 `lookup(프로필, 판본)`이 그 판본을 돌려주게 한다.
    pub fn remember(&self, snapshots: impl IntoIterator<Item = Arc<Snapshot>>) {
        self.history.lock().unwrap().extend(snapshots);
    }

    /// 판본을 복원하지 못하는 저장소를 흉내 낸다(활성이거나 기억한 판본만 복원된다).
    pub fn strict(&self, strict: bool) {
        *self.strict.lock().unwrap() = strict;
    }

    /// `active()`가 `calls`번 불린 다음부터 이 집합이 활성이 된다.
    pub fn flip_after(&self, calls: usize, snapshots: impl IntoIterator<Item = Arc<Snapshot>>) {
        *self.flip.lock().unwrap() = Some((calls, catalog(snapshots)));
    }

    /// 새 집합으로 한 번에 교체한다.
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
