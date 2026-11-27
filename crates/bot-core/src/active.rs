//! 이 프로세스가 쓰는 프로필 집합이 저장소가 게시한 활성 집합과 같은지 확인하는 계약과 기본 구현.
//!
//! 구판 프로세스는 새 접수·결과 반환·모델/도구 호출을 열지 않고 `Stale`로 닫아 새 판본의 프로세스가 이어 받게 한다.

use crate::{
    id::Revision,
    port::{Error, Reply},
    profile::ActiveRevisions,
};
use std::sync::{Arc, RwLock};

pub trait ActiveCheck: Send + Sync {
    /// 이 프로세스의 집합이 저장소의 활성 집합과 같으면 Ok, 다르거나 아직 게시되지 않았으면 `Stale`,
    /// 저장소를 읽지 못하면 `Unavailable`.
    fn verify(&self) -> Reply<'_, ()>;
}

/// 이 프로세스가 쓰는 프로필 집합의 판본을 저장소의 활성 판본과 견준다.
pub struct ActiveSet {
    store: Arc<dyn ActiveRevisions>,
    expected: Arc<RwLock<Revision>>,
}

impl ActiveSet {
    pub fn new(store: Arc<dyn ActiveRevisions>, expected: Revision) -> Self {
        Self::live(store, Arc::new(RwLock::new(expected)))
    }

    /// 실행 중 스킬·MCP 즉시 활성화로 갱신되는 공유 활성 집합 판본을 본다.
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

        // 저장소만 바뀌면 구판(`Stale`)이다
        *store.0.lock().unwrap() = Some(Revision("r2".into()));
        assert!(matches!(poll_now(check.verify()), Err(Error::Stale)));

        // 같은 프로세스의 즉시 활성화가 공유 판본을 갱신하면 다시 일치한다
        *shared.write().unwrap() = Revision("r2".into());
        assert!(poll_now(check.verify()).is_ok());
    }
}
