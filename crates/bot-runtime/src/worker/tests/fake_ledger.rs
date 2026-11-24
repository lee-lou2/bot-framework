//! 워커의 CLI 재개 시험에 사용하는 호출 원장.

use super::fakes::lock;
use bot_core::{
    command::JsonPayload,
    id::*,
    ledger::{
        ApprovalState, CallLedger, CallRecord, Decision, FinishCall, OpenApproval, StartCall,
        StoredApproval,
    },
    port::{Error, Reply},
    state::RunContext,
};
use std::{collections::BTreeMap, sync::Mutex};

/// 세션 복구가 읽는 두 가지만 가진다: 체크포인트(프런트가 쓴 응답 등)와 호출 기록. 나머지는 이 시험에서 쓰지 않는다.
#[derive(Default)]
pub(super) struct FakeLedger {
    pub steps: Mutex<BTreeMap<String, JsonPayload>>,
    pub calls: Mutex<BTreeMap<String, CallRecord>>,
    /// 저장소의 활성 판본(없으면 비활성 프로필)
    pub active: Mutex<Option<Revision>>,
    /// 켜면 읽기가 모두 일시 오류다
    pub down: std::sync::atomic::AtomicBool,
}

fn unused<T: Send + 'static>() -> Reply<'static, T> {
    Box::pin(async { Err(Error::Unavailable) })
}

impl CallLedger for FakeLedger {
    fn approval_context(&self, _: ApprovalId, _: PrincipalId) -> Reply<'_, RunContext> {
        unused()
    }
    fn context(&self, _: Attempt) -> Reply<'_, RunContext> {
        unused()
    }
    fn lookup(&self, _: Attempt, call: CallId) -> Reply<'_, Option<CallRecord>> {
        let down = self.down.load(std::sync::atomic::Ordering::SeqCst);
        let found = lock(&self.calls).get(&call.0).cloned();
        Box::pin(async move {
            if down {
                return Err(Error::Unavailable);
            }
            Ok(found)
        })
    }
    fn step(&self, _: Attempt, key: String) -> Reply<'_, Option<JsonPayload>> {
        let down = self.down.load(std::sync::atomic::Ordering::SeqCst);
        let found = lock(&self.steps).get(&key).cloned();
        Box::pin(async move {
            if down {
                return Err(Error::Unavailable);
            }
            Ok(found)
        })
    }
    fn active_revision(&self, _: Attempt) -> Reply<'_, Option<Revision>> {
        let down = self.down.load(std::sync::atomic::Ordering::SeqCst);
        let active = lock(&self.active).clone();
        Box::pin(async move {
            if down {
                return Err(Error::Unavailable);
            }
            Ok(active)
        })
    }
    fn begin(&self, _: Attempt, _: CallId, _: ToolId, _: [u8; 32]) -> Reply<'_, CallRecord> {
        unused()
    }
    fn open_approval(&self, _: OpenApproval) -> Reply<'_, ()> {
        unused()
    }
    fn approval(&self, _: ApprovalId) -> Reply<'_, StoredApproval> {
        unused()
    }
    fn supersede(&self, _: ApprovalId, _: PrincipalId) -> Reply<'_, ()> {
        unused()
    }
    fn decide(&self, _: Decision) -> Reply<'_, ApprovalState> {
        unused()
    }
    fn start(&self, _: StartCall) -> Reply<'_, ()> {
        unused()
    }
    fn finish(&self, _: FinishCall) -> Reply<'_, ()> {
        unused()
    }
}
