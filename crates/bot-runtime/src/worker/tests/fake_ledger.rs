//! Minimal fake `CallLedger` backing worker CLI session-resume unit tests.

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

#[derive(Default)]
pub(super) struct FakeLedger {
    pub steps: Mutex<BTreeMap<String, JsonPayload>>,
    pub calls: Mutex<BTreeMap<String, CallRecord>>,
    pub active: Mutex<Option<Revision>>,
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
    fn start_with(
        &self,
        _: StartCall,
        _: bot_core::grant::Authority,
    ) -> Reply<'_, bot_core::grant::Granted> {
        Box::pin(async { Err(Error::Denied) })
    }
    fn evidence(&self, _: Attempt) -> Reply<'_, Option<bot_core::grant::Evidence>> {
        Box::pin(async { Ok(None) })
    }
    fn finish(&self, _: FinishCall) -> Reply<'_, ()> {
        unused()
    }
    fn finish_blocked(&self, finish: FinishCall, _: bot_core::api::ApprovalBlock) -> Reply<'_, ()> {
        self.finish(finish)
    }
    fn finish_pass(
        &self,
        _: FinishCall,
        _: u32,
        _: std::collections::BTreeSet<AgentId>,
    ) -> Reply<'_, bot_core::pass::PassNext> {
        unused()
    }
}
