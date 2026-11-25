//! OpenAI 출처 승인 시험용 기록: 카드를 보낼 전달 바인딩, 확정한 게시 영수증, 호출을 막은 이유. 실제 저장소는 같은 값을 승인·호출 기록과 같은 커밋에 저장한다.

use bot_core::{api::ApprovalBlock, cards::CardReceipt, id::*, ledger::StoredApproval};
use std::{collections::HashMap, sync::Mutex};

#[derive(Default)]
struct Inner {
    delivery: HashMap<ApprovalId, BindingId>,
    receipts: HashMap<ApprovalId, CardReceipt>,
    blocked: HashMap<(RunId, CallId), ApprovalBlock>,
}

#[derive(Default)]
pub(super) struct Notes(Mutex<Inner>);

impl Notes {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn opened(&self, nonce: &ApprovalId, delivery: Option<&BindingId>) {
        if let Some(binding) = delivery {
            self.inner().delivery.insert(nonce.clone(), binding.clone());
        }
    }

    pub(super) fn blocked(&self, run: &RunId, call: &CallId, why: ApprovalBlock) {
        self.inner()
            .blocked
            .insert((run.clone(), call.clone()), why);
    }

    /// 게시를 확인한 카드면 그 영수증을 싣는다.
    pub(super) fn with_receipt(
        &self,
        mut stored: StoredApproval,
        nonce: &ApprovalId,
    ) -> StoredApproval {
        stored.delivery = self.inner().receipts.get(nonce).cloned();
        stored
    }

    pub(super) fn delivery(&self, nonce: &ApprovalId) -> Option<BindingId> {
        self.inner().delivery.get(nonce).cloned()
    }

    pub(super) fn why(&self, run: &str, call: &str) -> Option<ApprovalBlock> {
        let key = (RunId(run.into()), CallId(call.into()));
        self.inner().blocked.get(&key).copied()
    }

    pub(super) fn post(&self, nonce: &ApprovalId, receipt: CardReceipt) {
        self.inner().receipts.insert(nonce.clone(), receipt);
    }

    pub(super) fn withdraw(&self, nonce: &ApprovalId) {
        self.inner().receipts.remove(nonce);
    }
}
