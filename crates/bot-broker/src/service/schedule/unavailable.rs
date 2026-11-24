use bot_core::{
    command::JsonPayload,
    id::{Attempt, CallId, Revision, ScheduleId},
    ledger::StartCall,
    port::{Error, Reply},
    schedule::{
        AdvanceSchedule, DueKey, DuePage, ScheduleSpec, ScheduleSummary, UserSchedules, Wake,
    },
};

/// 저장소를 붙이지 않은 브로커: 모든 예약 연산이 `Unavailable`로 닫힌다.
pub struct NoSchedules;

impl UserSchedules for NoSchedules {
    fn create(&self, _: StartCall, _: ScheduleSpec) -> Reply<'_, ScheduleId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn list(&self, _: Attempt, _: Revision) -> Reply<'_, Vec<ScheduleSummary>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn cancel(&self, _: StartCall, _: ScheduleId) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn finish_with_wake(
        &self,
        _: Attempt,
        _: CallId,
        _: Revision,
        _: JsonPayload,
        _: Wake,
    ) -> Reply<'_, ScheduleId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn due(&self, _: Option<DueKey>, _: u32) -> Reply<'_, DuePage> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn advance(&self, _: AdvanceSchedule) -> Reply<'_, Option<bot_core::id::RunId>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}
