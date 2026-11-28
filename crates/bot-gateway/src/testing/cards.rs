//! In-memory `CardOutbox` test double for owner-DM approval card delivery.
use crate::testing::TestCatalog;
use crate::testing::ports::MemoryLedger;
use bot_core::{
    api::ApprovalBlock,
    cards::{CardJob, CardKey, CardOrigin, CardOutbox, CardOutcome, CardReceipt},
    id::{ApprovalId, Revision},
    intake::PushTransportId,
    port::{Error, Reply},
    profile::ProfileCatalog,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

pub struct MemoryCards {
    ledger: Option<Arc<MemoryLedger>>,
    inner: Mutex<Inner>,
    fence: Mutex<Option<Arc<TestCatalog>>>,
    before_begin: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct Inner {
    known: BTreeSet<ApprovalId>,
    pending: Vec<CardJob>,
    leased: Vec<CardJob>,
    begun: Vec<(CardKey, String)>,
    settled: Vec<(String, CardOutcome)>,
    receipts: BTreeMap<String, CardReceipt>,
    fail_begin: bool,
    fail_claim: bool,
    fail_settle: Vec<Error>,
    origins: BTreeMap<String, CardOrigin>,
    fail_origin: bool,
    blocked: Vec<(String, ApprovalBlock)>,
}

impl MemoryCards {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ledger: None,
            inner: Mutex::default(),
            fence: Mutex::new(None),
            before_begin: Mutex::new(None),
        })
    }

    pub fn linked(ledger: Arc<MemoryLedger>) -> Arc<Self> {
        Arc::new(Self {
            ledger: Some(ledger),
            inner: Mutex::default(),
            fence: Mutex::new(None),
            before_begin: Mutex::new(None),
        })
    }

    /// Enables revision fencing in `begin` against `catalog` (returning `Stale` on mismatch).
    pub fn fence(&self, catalog: Arc<TestCatalog>) {
        *self.fence.lock().unwrap() = Some(catalog);
    }

    pub fn before_begin(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.before_begin.lock().unwrap() = Some(Box::new(hook));
    }

    pub fn receipt(&self, nonce: &ApprovalId) -> Option<CardReceipt> {
        self.inner.lock().unwrap().receipts.get(&nonce.0).cloned()
    }

    pub fn push(&self, job: CardJob) {
        let mut inner = self.inner.lock().unwrap();
        if inner.known.insert(job.key.nonce.clone()) {
            inner.pending.push(job);
        }
    }

    pub fn begun(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .unwrap()
            .begun
            .iter()
            .map(|(key, revision)| (key.nonce.0.clone(), revision.clone()))
            .collect()
    }

    pub fn settled(&self) -> Vec<(String, CardOutcome)> {
        self.inner.lock().unwrap().settled.clone()
    }

    pub fn set_origin(&self, nonce: &str, origin: CardOrigin) {
        self.inner
            .lock()
            .unwrap()
            .origins
            .insert(nonce.to_owned(), origin);
    }

    pub fn fail_origin(&self, fail: bool) {
        self.inner.lock().unwrap().fail_origin = fail;
    }

    pub fn blocked(&self) -> Vec<(String, ApprovalBlock)> {
        self.inner.lock().unwrap().blocked.clone()
    }

    pub fn fail_begin(&self, fail: bool) {
        self.inner.lock().unwrap().fail_begin = fail;
    }

    pub fn fail_claim(&self, fail: bool) {
        self.inner.lock().unwrap().fail_claim = fail;
    }

    pub fn fail_settle(&self, errors: impl IntoIterator<Item = Error>) {
        self.inner.lock().unwrap().fail_settle = errors.into_iter().collect();
    }

    pub fn waiting(&self) -> usize {
        self.inner.lock().unwrap().pending.len()
    }
}

fn ready<T: Send + 'static>(value: Result<T, Error>) -> Reply<'static, T> {
    Box::pin(std::future::ready(value))
}

impl CardOutbox for MemoryCards {
    fn claim(&self, limit: u32) -> Reply<'_, Vec<CardJob>> {
        self.claim_for(&PushTransportId::SLACK, limit)
    }

    fn claim_for<'a>(
        &'a self,
        transport: &'a PushTransportId,
        limit: u32,
    ) -> Reply<'a, Vec<CardJob>> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_claim {
            return ready(Err(Error::Unavailable));
        }
        let want_telegram = *transport == PushTransportId::TELEGRAM;
        let mut taken = Vec::new();
        let mut remaining = Vec::new();
        for mut job in std::mem::take(&mut inner.pending) {
            let is_telegram = job.owner.0.starts_with("telegram:");
            if taken.len() < limit as usize && is_telegram == want_telegram {
                job.key.attempt += 1;
                taken.push(job);
            } else {
                remaining.push(job);
            }
        }
        inner.pending = remaining;
        inner.leased.extend(taken.clone());
        ready(Ok(taken))
    }

    fn begin(&self, key: CardKey, checked_revision: Revision) -> Reply<'_, ()> {
        if let Some(hook) = self.before_begin.lock().unwrap().as_ref() {
            hook();
        }
        let fence = self.fence.lock().unwrap().clone();
        let mut inner = self.inner.lock().unwrap();
        if let Some(catalog) = fence {
            let profile = inner
                .leased
                .iter()
                .find(|job| job.key == key)
                .map(|job| job.profile.clone());
            let current = profile.and_then(|profile| {
                catalog
                    .active()
                    .get(&profile)
                    .map(|snapshot| snapshot.revision.clone())
            });
            if current.as_ref() != Some(&checked_revision) {
                return ready(Err(Error::Stale));
            }
        }
        if !inner.leased.iter().any(|job| job.key == key) {
            return ready(Err(Error::Stale));
        }
        let first = !inner.begun.iter().any(|(begun, _)| *begun == key);
        if inner.fail_begin || !first {
            return ready(Err(Error::Conflict));
        }
        inner.begun.push((key, checked_revision.0));
        ready(Ok(()))
    }

    fn settle(&self, key: CardKey, outcome: CardOutcome) -> Reply<'_, ()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.fail_settle.is_empty() {
            return ready(Err(inner.fail_settle.remove(0)));
        }
        let Some(at) = inner.leased.iter().position(|job| job.key == key) else {
            return ready(Err(Error::Stale));
        };
        let job = inner.leased.remove(at);
        match &outcome {
            CardOutcome::Posted { team, dm, ts } => {
                let receipt = CardReceipt {
                    team: team.clone(),
                    dm: dm.clone(),
                    ts: ts.clone(),
                    owner: job.owner.clone(),
                    profile: job.profile.clone(),
                    binding: job.binding.clone(),
                    source: job.source.clone(),
                };
                if let Some(ledger) = &self.ledger {
                    ledger.deliver(&key.nonce.0, receipt.clone());
                }
                inner.receipts.insert(key.nonce.0.clone(), receipt);
            }
            // Re-queue transient `NotSent` attempts and clear `begun` so the next attempt can begin.
            CardOutcome::NotSent { .. } => {
                inner.begun.retain(|(begun, _)| *begun != key);
                inner.pending.push(job);
            }
            CardOutcome::Unavailable | CardOutcome::Held => {}
        }
        inner.settled.push((key.nonce.0, outcome));
        ready(Ok(()))
    }

    fn origin(&self, nonce: ApprovalId) -> Reply<'_, Option<CardOrigin>> {
        let inner = self.inner.lock().unwrap();
        if inner.fail_origin {
            return ready(Err(Error::Unavailable));
        }
        ready(Ok(inner.origins.get(&nonce.0).cloned()))
    }

    fn block(&self, key: CardKey, why: ApprovalBlock) -> Reply<'_, ()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.fail_settle.is_empty() {
            return ready(Err(inner.fail_settle.remove(0)));
        }
        let Some(at) = inner.leased.iter().position(|job| job.key == key) else {
            return ready(Err(Error::Stale));
        };
        inner.leased.remove(at);
        inner.blocked.push((key.nonce.0, why));
        ready(Ok(()))
    }
}
