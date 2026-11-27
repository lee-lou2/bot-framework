//! `UserSchedules` 가짜(메모리). 실제 저장소가 지켜야 할 규칙을 그대로 적었다.
//! - 생성·취소는 승인 소비·호출 완료를 한 번에 한다(`MemLedger::complete_builtin`). 소유자(요청자)별 활성 예약은 32개까지다.
//! - 목록은 현재 claim과 활성 판본을 확인하고 본인 활성 예약만 ID 순서로 준다.
//! - `finish_with_wake`는 `Started` 호출의 결과와 (run, call)에 유일한 후속 예약을 한 번에 저장한다. 같은 호출의 재전달은 같은 예약이다.

use super::MemLedger;
use bot_core::{
    command::JsonPayload,
    id::*,
    ledger::{CallLedger, StartCall},
    port::{Error, Reply},
    schedule::{
        AdvanceSchedule, DeliveryTarget, DueKey, DuePage, MAX_ACTIVE_PER_OWNER,
        MAX_INSTRUCTION_BYTES, ScheduleSpec, ScheduleSummary, Timing, UserSchedules, Wake,
    },
};
use std::{
    sync::{Arc, Mutex},
    time::SystemTime,
};

#[derive(Clone)]
struct Entry {
    id: ScheduleId,
    profile: ProfileId,
    owner: PrincipalId,
    timing: Timing,
    instruction: String,
    quiet: bool,
    /// 이 예약을 만든 호출(후속 예약은 부모 도구 호출).
    origin: (RunId, CallId),
    /// 후속 확인이면 true.
    wake: bool,
    /// 명시 목적지(다른 수신자). 없으면 원래 대화다.
    destination: Option<DeliveryTarget>,
}

pub struct MemSchedules {
    ledger: Arc<MemLedger>,
    entries: Mutex<Vec<Entry>>,
    next: Mutex<u64>,
    /// `finish_with_wake`가 이 오류로 실패한다(저장소 장애).
    wake_failure: Mutex<Option<Error>>,
    /// 켜면 `finish_with_wake`가 커밋한 뒤 응답을 잃는다(`Unavailable`).
    lose_wake_response: Mutex<bool>,
    /// `list`가 이 오류로 실패한다(저장소 장애).
    list_failure: Mutex<Option<Error>>,
}

use super::lock;

/// 저장소가 받는 시간대 이름(시험용: 진짜 IANA 검증은 저장소가 한다).
fn known_timezone(name: &str) -> bool {
    matches!(
        name,
        "UTC" | "Asia/Seoul" | "America/New_York" | "Europe/Berlin"
    )
}

impl MemSchedules {
    pub fn new(ledger: Arc<MemLedger>) -> Self {
        Self {
            ledger,
            entries: Mutex::default(),
            next: Mutex::new(0),
            wake_failure: Mutex::new(None),
            lose_wake_response: Mutex::new(false),
            list_failure: Mutex::new(None),
        }
    }

    /// 이후 `list`가 이 오류로 실패한다(`None`이면 다시 정상).
    pub fn fail_list_with(&self, error: Option<Error>) {
        *lock(&self.list_failure) = error;
    }

    /// 이후 `finish_with_wake`가 이 오류로 실패한다(`None`이면 다시 정상).
    pub fn fail_wake_with(&self, error: Option<Error>) {
        *lock(&self.wake_failure) = error;
    }

    /// 켜면 `finish_with_wake`가 커밋한 뒤 응답을 잃는다.
    pub fn lose_wake_response(&self, lose: bool) {
        *lock(&self.lose_wake_response) = lose;
    }

    /// 지금 활성인 예약 수(전체).
    pub fn count(&self) -> usize {
        lock(&self.entries).len()
    }

    /// 후속 확인으로 생긴 예약 `(지시문, 시각, quiet)`.
    pub fn wakes(&self) -> Vec<(String, SystemTime, bool)> {
        lock(&self.entries)
            .iter()
            .filter(|e| e.wake)
            .filter_map(|e| match e.timing {
                Timing::At(at) => Some((e.instruction.clone(), at, e.quiet)),
                Timing::Cron { .. } => None,
            })
            .collect()
    }

    /// 저장된 예약 한 건의 `(지시문, 시간 설정, quiet)`.
    pub fn get(&self, id: &str) -> Option<(String, Timing, bool)> {
        lock(&self.entries)
            .iter()
            .find(|e| e.id.0 == id)
            .map(|e| (e.instruction.clone(), e.timing.clone(), e.quiet))
    }

    /// 저장된 예약의 명시 목적지(원래 대화면 `None`).
    pub fn destination(&self, id: &str) -> Option<DeliveryTarget> {
        lock(&self.entries)
            .iter()
            .find(|e| e.id.0 == id)
            .and_then(|e| e.destination.clone())
    }

    /// 한 소유자의 활성 예약을 채워 한도를 시험한다.
    pub fn fill(&self, profile: &str, owner: &str, n: usize) {
        for _ in 0..n {
            let id = self.fresh_id();
            lock(&self.entries).push(Entry {
                id,
                profile: ProfileId(profile.into()),
                owner: PrincipalId(owner.into()),
                timing: Timing::At(
                    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000),
                ),
                instruction: "filler".into(),
                quiet: false,
                origin: (RunId("filler".into()), CallId("filler".into())),
                wake: false,
                destination: None,
            });
        }
    }

    fn fresh_id(&self) -> ScheduleId {
        let mut next = lock(&self.next);
        *next += 1;
        ScheduleId(format!("sched-{:03}", *next))
    }

    fn active_for(&self, profile: &ProfileId, owner: &PrincipalId) -> usize {
        lock(&self.entries)
            .iter()
            .filter(|e| e.profile == *profile && e.owner == *owner)
            .count()
    }
}

fn spec_ok(spec: &ScheduleSpec) -> Result<(), Error> {
    if spec.instruction.trim().is_empty() || spec.instruction.len() > MAX_INSTRUCTION_BYTES {
        return Err(Error::Invalid);
    }
    if let Timing::Cron { timezone, .. } = &spec.timing
        && !known_timezone(timezone)
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

impl UserSchedules for MemSchedules {
    fn create(&self, start: StartCall, spec: ScheduleSpec) -> Reply<'_, ScheduleId> {
        Box::pin(async move {
            spec_ok(&spec)?;
            let owner = self.ledger.principal_of(&start.attempt)?;
            let profile = start.attempt.profile.clone();
            let origin = (start.attempt.run.clone(), start.call.clone());
            // 같은 호출의 재전달은 같은 예약이다.
            if let Some(existing) = lock(&self.entries)
                .iter()
                .find(|e| e.origin == origin && !e.wake)
            {
                return Ok(existing.id.clone());
            }
            if self.active_for(&profile, &owner) >= MAX_ACTIVE_PER_OWNER {
                return Err(Error::Conflict);
            }
            let id = self.fresh_id();
            let result = format!("{{\"schedule\":\"{}\"}}", id.0);
            if self
                .ledger
                .complete_builtin(start, "schedule.create", &result)?
            {
                lock(&self.entries).push(Entry {
                    id: id.clone(),
                    profile,
                    owner,
                    timing: spec.timing,
                    instruction: spec.instruction,
                    quiet: spec.quiet,
                    origin,
                    wake: false,
                    destination: spec.destination,
                });
            }
            Ok(id)
        })
    }

    fn list(&self, attempt: Attempt, checked: Revision) -> Reply<'_, Vec<ScheduleSummary>> {
        Box::pin(async move {
            if let Some(error) = lock(&self.list_failure).as_ref() {
                return Err(match error {
                    Error::Stale => Error::Stale,
                    Error::Denied => Error::Denied,
                    _ => Error::Unavailable,
                });
            }
            self.ledger.check_current(&attempt, &checked)?;
            let owner = self.ledger.principal_of(&attempt)?;
            let mut mine: Vec<ScheduleSummary> = lock(&self.entries)
                .iter()
                .filter(|e| e.profile == attempt.profile && e.owner == owner)
                .map(|e| ScheduleSummary {
                    id: e.id.clone(),
                    timing: e.timing.clone(),
                    quiet: e.quiet,
                })
                .collect();
            mine.sort_by(|a, b| a.id.cmp(&b.id));
            mine.truncate(MAX_ACTIVE_PER_OWNER);
            Ok(mine)
        })
    }

    fn cancel(&self, start: StartCall, id: ScheduleId) -> Reply<'_, ()> {
        Box::pin(async move {
            let owner = self.ledger.principal_of(&start.attempt)?;
            let profile = start.attempt.profile.clone();
            let found = lock(&self.entries)
                .iter()
                .any(|e| e.id == id && e.profile == profile && e.owner == owner);
            if !found {
                // 이미 이 호출이 취소를 끝냈다면(재전달) 같은 결과다. 아니면 본인 예약이 아니다.
                let result = format!("{{\"schedule\":\"{}\"}}", id.0);
                return match self
                    .ledger
                    .complete_builtin(start, "schedule.cancel", &result)
                {
                    Ok(false) => Ok(()),
                    _ => Err(Error::Missing),
                };
            }
            let result = format!("{{\"schedule\":\"{}\"}}", id.0);
            self.ledger
                .complete_builtin(start, "schedule.cancel", &result)?;
            lock(&self.entries).retain(|e| e.id != id);
            Ok(())
        })
    }

    fn finish_with_wake(
        &self,
        attempt: Attempt,
        call: CallId,
        checked: Revision,
        result: JsonPayload,
        wake: Wake,
    ) -> Reply<'_, ScheduleId> {
        Box::pin(async move {
            if let Some(error) = lock(&self.wake_failure).as_ref() {
                return Err(match error {
                    Error::Stale => Error::Stale,
                    Error::Denied => Error::Denied,
                    Error::Conflict => Error::Conflict,
                    _ => Error::Unavailable,
                });
            }
            if wake.instruction.trim().is_empty() || wake.instruction.len() > MAX_INSTRUCTION_BYTES
            {
                return Err(Error::Invalid);
            }
            let owner = self.ledger.principal_of(&attempt)?;
            let profile = attempt.profile.clone();
            let origin = (attempt.run.clone(), call.clone());
            // 같은 호출을 다시 끝내려 하면(재전달) 이미 만든 후속 예약이다.
            if let Some(existing) = lock(&self.entries)
                .iter()
                .find(|e| e.origin == origin && e.wake)
            {
                return Ok(existing.id.clone());
            }
            if self.active_for(&profile, &owner) >= MAX_ACTIVE_PER_OWNER {
                return Err(Error::Conflict);
            }
            self.ledger
                .finish_started(&attempt, &call, &checked, &result.0)?;
            let id = self.fresh_id();
            lock(&self.entries).push(Entry {
                id: id.clone(),
                profile,
                owner,
                timing: Timing::At(wake.at),
                instruction: wake.instruction,
                quiet: wake.quiet,
                origin,
                wake: true,
                destination: None,
            });
            if *lock(&self.lose_wake_response) {
                return Err(Error::Unavailable); // 커밋했지만 응답을 잃었다
            }
            Ok(id)
        })
    }

    fn due(&self, _: Option<DueKey>, _: u32) -> Reply<'_, DuePage> {
        Box::pin(async { Err(Error::Unavailable) })
    }

    fn advance(&self, _: AdvanceSchedule) -> Reply<'_, Option<RunId>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

#[allow(dead_code)]
fn _uses_call_ledger(_: &dyn CallLedger) {}
