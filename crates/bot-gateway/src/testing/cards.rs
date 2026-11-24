//! 소유자 DM 승인 카드 전달(`bot_core::cards`)의 메모리 저장소: 작업을 점유하고(`claim`), 최초 전송자만 `begin`을 받고, 정산을 기록한다. `Posted`면 영수증을 만들고
//! (원장 가짜에 이어 두었으면 그 승인의 `delivery`로도 읽힌다 — 실제 저장소가 정산과 한 커밋에 하는 일), 일시 실패(`NotSent`)는 다시 잡히도록 되돌린다(백오프 시간은 흉내 내지 않는다).
use crate::testing::ports::MemoryLedger;
use bot_core::{
    cards::{CardJob, CardOutbox, CardOutcome, CardReceipt},
    id::{ApprovalId, Revision},
    port::{Error, Reply},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub struct MemoryCards {
    ledger: Option<Arc<MemoryLedger>>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    pending: Vec<CardJob>,
    leased: Vec<CardJob>,
    begun: Vec<(String, String)>,
    settled: Vec<(String, CardOutcome)>,
    receipts: BTreeMap<String, CardReceipt>,
    fail_begin: bool,
    fail_claim: bool,
    /// 정산이 차례로 받을 오류(앞에서부터 하나씩 쓰고, 비면 정상으로 기록한다)
    fail_settle: Vec<Error>,
}

impl MemoryCards {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ledger: None,
            inner: Mutex::default(),
        })
    }

    /// `Posted` 정산이 영수증을 이 원장의 승인(`delivery`)에도 남긴다.
    pub fn linked(ledger: Arc<MemoryLedger>) -> Arc<Self> {
        Arc::new(Self {
            ledger: Some(ledger),
            inner: Mutex::default(),
        })
    }

    /// 정산이 만든 영수증
    pub fn receipt(&self, nonce: &ApprovalId) -> Option<CardReceipt> {
        self.inner.lock().unwrap().receipts.get(&nonce.0).cloned()
    }

    pub fn push(&self, job: CardJob) {
        self.inner.lock().unwrap().pending.push(job);
    }

    /// `begin`이 기록한 (승인 식별자, 확인한 활성 판본)
    pub fn begun(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().begun.clone()
    }

    /// 정산 기록 (승인 식별자, 결과) — 정산 순서대로
    pub fn settled(&self) -> Vec<(String, CardOutcome)> {
        self.inner.lock().unwrap().settled.clone()
    }

    pub fn fail_begin(&self, fail: bool) {
        self.inner.lock().unwrap().fail_begin = fail;
    }

    pub fn fail_claim(&self, fail: bool) {
        self.inner.lock().unwrap().fail_claim = fail;
    }

    /// 다음 정산들을 이 오류들로 차례로 거절한다(기록하지 않는다).
    pub fn fail_settle(&self, errors: impl IntoIterator<Item = Error>) {
        self.inner.lock().unwrap().fail_settle = errors.into_iter().collect();
    }

    /// 아직 점유되지 않고 기다리는 작업 수
    pub fn waiting(&self) -> usize {
        self.inner.lock().unwrap().pending.len()
    }
}

fn ready<T: Send + 'static>(value: Result<T, Error>) -> Reply<'static, T> {
    Box::pin(std::future::ready(value))
}

impl CardOutbox for MemoryCards {
    fn claim(&self, limit: u32) -> Reply<'_, Vec<CardJob>> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_claim {
            return ready(Err(Error::Unavailable));
        }
        let take = inner.pending.len().min(limit as usize);
        let taken: Vec<CardJob> = inner.pending.drain(..take).collect();
        inner.leased.extend(taken.clone());
        ready(Ok(taken))
    }

    fn begin(&self, nonce: ApprovalId, checked_revision: Revision) -> Reply<'_, ()> {
        let mut inner = self.inner.lock().unwrap();
        let held = inner.leased.iter().any(|job| job.nonce == nonce);
        let first = !inner.begun.iter().any(|(begun, _)| *begun == nonce.0);
        if inner.fail_begin || !held || !first {
            return ready(Err(Error::Conflict));
        }
        inner.begun.push((nonce.0, checked_revision.0));
        ready(Ok(()))
    }

    fn settle(&self, nonce: ApprovalId, outcome: CardOutcome) -> Reply<'_, ()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.fail_settle.is_empty() {
            return ready(Err(inner.fail_settle.remove(0)));
        }
        let Some(at) = inner.leased.iter().position(|job| job.nonce == nonce) else {
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
                };
                if let Some(ledger) = &self.ledger {
                    ledger.deliver(&nonce.0, receipt.clone());
                }
                inner.receipts.insert(nonce.0.clone(), receipt);
            }
            // 아무것도 올라가지 않았다: 다시 잡히게 되돌리고 `begin` 기록을 지운다(다음 시도가 최초 전송자다)
            CardOutcome::NotSent { .. } => {
                inner.begun.retain(|(begun, _)| *begun != nonce.0);
                inner.pending.push(job);
            }
            CardOutcome::Unavailable | CardOutcome::Held => {}
        }
        inner.settled.push((nonce.0, outcome));
        ready(Ok(()))
    }
}
