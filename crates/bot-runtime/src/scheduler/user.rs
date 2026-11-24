//! 사용자 예약(`UserSchedules`)의 발화. 저장소가 DB 시각까지 확인할 예약을 주면, 현재 활성 판본에서 멤버·바인딩·에이전트·권한을 다시 보고
//! 다음에 할 일(발화·기다림·닫기)을 정해 `advance`로 한 번에 쓴다. 두 스케줄러가 같은 예약을 보아도 저장소가 같은 발생을 같은 run으로 수렴시킨다.
//!
//! - 권한은 발화 시점의 값이다: 만든 때의 상한 ∩ 지금 판본에서 소유자가 에이전트로 쓸 수 있는 권한. `run`이 없으면 닫는다.
//! - cron은 놓친 발생을 오래된 순으로 하나씩 접수한다(사용자 지시를 묵시적으로 버리지 않는다). 남은 발생이 있으면 곧바로 다시 점검한다.
//! - 한 바퀴(`due`가 처음부터 끝 행까지 읽는 동안)에서 예약마다 **한 번씩만** 차례를 준다. 밀린 반복 예약은 발화할 때마다 다음 확인 시각이 앞으로 가서 같은 바퀴의 뒤쪽 쪽에
//!   다시 나타나는데, 그때마다 발화하면 다른 예약이 차례를 얻지 못한다. 남은 발생은 다음 바퀴에서 이어 간다(바퀴가 끝나면 다음 점검이 처음부터 다시 읽는다).
//!   「남은 발생이 있다」는 쪽을 넘어서도 사라지지 않는다: 바퀴 안에서 바로 누적해 두었다가 바퀴가 끝날 때 한 번 전한다(다음 바퀴를 곧바로 시작한다).
//! - 해석할 수 없는 시간대·cron은 닫는다(고정 코드로 한 번 기록). 저장된 값(cron · 목적지)을 읽지 못한 행은 `DuePage::unreadable`로 따로 오며, 그 행만 닫고 쪽의 나머지는 그대로 본다.
//!   정상 행과 손상 행이 섞인 쪽은 저장소가 읽은 순서(`(다음 확인 시각, 프로필, ID)`) 그대로 처리한다 — 뒤에 온 손상 행의 잠금을 기다리느라 앞의 정상 행이 밀리지 않는다.

use super::Scheduler;
use crate::{cron::Cron, scope};
use bot_core::{
    id::{ProfileId, Revision, ScheduleId},
    port::Error,
    profile::{Scopes, Snapshot},
    schedule::{
        AdvanceSchedule, CloseReason, DeliveryTarget, DueKey, DuePage, DueSchedule, NextOccurrence,
        Timing, UnreadableSchedule,
    },
};
use chrono_tz::Tz;
use std::{collections::HashSet, time::SystemTime};

/// 한 바퀴를 이어 읽는 상태. 바퀴는 처음(`after`가 없음)에서 시작해 끝 행까지 읽으면(`next`가 없음) 끝난다.
#[derive(Default)]
pub(super) struct Pass {
    /// 직전 점검이 읽은 쪽의 마지막 자리. None이면 처음(가장 오래된 것)부터 읽는다.
    after: Option<DueKey>,
    /// 이번 바퀴에서 이미 차례를 준 예약. 바퀴가 끝나면 비운다 — 다음 바퀴에는 다시 기회를 준다.
    served: HashSet<(ProfileId, ScheduleId)>,
    /// 이번 바퀴에서 발화하고도 지금 확인할 발생이 더 남은 예약이 있었다(밀린 반복 예약). 발화 직후 바로 쌓고(쪽을 넘어도 잃지 않는다), 바퀴가 끝날 때 다음 바퀴를 곧바로 시작하라고 한 번 전한 뒤 비운다.
    more_due: bool,
}

/// 한 쪽의 한 행: 정상 예약 또는 저장된 값을 읽지 못한 예약.
enum Row<'a> {
    Ready(&'a DueSchedule),
    Broken(&'a UnreadableSchedule),
}

/// 쪽의 정상 행과 손상 행을 저장소가 읽은 순서 `(다음 확인 시각, 프로필, ID)`로 합친다.
fn in_page_order(page: &DuePage) -> Vec<Row<'_>> {
    let mut rows: Vec<(SystemTime, &ProfileId, &ScheduleId, Row<'_>)> = page
        .schedules
        .iter()
        .map(|s| (s.at, &s.profile, &s.id, Row::Ready(s)))
        .chain(
            page.unreadable
                .iter()
                .map(|u| (u.at, &u.profile, &u.id, Row::Broken(u))),
        )
        .collect();
    rows.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    rows.into_iter().map(|(_, _, _, row)| row).collect()
}

/// 한 번의 점검 결과.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UserTick {
    /// 접수한(또는 이미 접수돼 있던) 발생 수
    pub fired: usize,
    /// 첫 발생까지 기다리게 한 예약 수
    pub waiting: usize,
    /// 닫은 예약 수(멤버·바인딩·에이전트·권한 철회, 비활성 프로필, 남은 발생 없음)
    pub closed: usize,
    /// 한 번에 다 못 봤다: 곧바로 다시 점검한다
    pub backlog: bool,
}

/// 이 예약을 지금 어떻게 할지: (확인한 판본, 다음 일). 프로필이 활성이 아니면 판본이 없다.
pub(crate) fn plan(
    s: &DueSchedule,
    snapshot: Option<&Snapshot>,
    now: SystemTime,
) -> (Option<Revision>, NextOccurrence) {
    let Some(snapshot) = snapshot else {
        return (None, NextOccurrence::Close(CloseReason::ProfileInactive));
    };
    let revision = Some(snapshot.revision.clone());
    if !snapshot.members.contains_key(&s.owner) {
        return (revision, NextOccurrence::Close(CloseReason::MemberRevoked));
    }
    if !snapshot.bindings.contains_key(&s.origin.binding) {
        return (revision, NextOccurrence::Close(CloseReason::BindingRevoked));
    }
    if !snapshot.agents.contains_key(&s.agent) {
        return (revision, NextOccurrence::Close(CloseReason::AgentRevoked));
    }
    let scopes: Scopes = scope::effective(snapshot, &s.owner, &s.agent)
        .map(|now_allowed| s.ceiling.intersection(&now_allowed).cloned().collect())
        .unwrap_or_default();
    if !scopes.contains("run") {
        return (
            revision,
            NextOccurrence::Close(CloseReason::PermissionRevoked),
        );
    }
    let next = match &s.spec.timing {
        Timing::At(at) if *at > now => NextOccurrence::WaitUntil(*at),
        Timing::At(at) => NextOccurrence::Fire {
            at: *at,
            next: None,
            scopes,
        },
        Timing::Cron {
            expression,
            timezone,
        } => {
            let (Ok(cron), Ok(zone)) =
                (Cron::parse(&expression.to_string()), timezone.parse::<Tz>())
            else {
                return (revision, NextOccurrence::Close(CloseReason::InvalidTiming));
            };
            match cron.next_after(zone, s.cursor) {
                None => NextOccurrence::Close(CloseReason::Finished),
                Some(first) if first > now => NextOccurrence::WaitUntil(first),
                Some(first) => NextOccurrence::Fire {
                    at: first,
                    next: cron.next_after(zone, first),
                    scopes,
                },
            }
        }
    };
    (revision, next)
}

/// 명시 목적지를 발화 전에 현재 등록부로 다시 본 결과.
enum Route {
    /// 봉인한 그대로의 같은 경로다
    Same,
    /// 없어졌거나 다른 팀·채널로 재지정됐다: 닫는다(발화 0)
    Gone,
    /// 확인하지 못했다(등록부가 없거나 일시 오류): 발화도 닫기도 하지 않고 다음 점검에서 다시 본다
    Unsure,
}

fn same_target(a: &DeliveryTarget, b: &DeliveryTarget) -> bool {
    a.binding == b.binding
        && a.conversation.kind == b.conversation.kind
        && a.conversation.id == b.conversation.id
        && a.conversation.thread == b.conversation.thread
}

impl Scheduler {
    /// 명시 목적지가 지금 운영자 등록부에서도 봉인한 그대로인지 본다(원래 대화 예약에는 부르지 않는다).
    fn route(&self, profile: &ProfileId, target: &DeliveryTarget) -> Route {
        let Some(routes) = &self.routes else {
            return Route::Unsure;
        };
        match routes.resolve(profile, target) {
            Ok(resolved) if same_target(&resolved, target) => Route::Same,
            Ok(_) | Err(Error::Denied | Error::Invalid) => Route::Gone,
            Err(_) => Route::Unsure,
        }
    }

    /// 사용자 예약을 한 번 점검한다. 저장소가 응답하지 못하면 오류(다음 점검에서 같은 쪽부터 이어 간다).
    /// 확인할 예약은 한 쪽씩 **이어서** 읽는다: 직전 점검이 읽은 쪽의 마지막 `(다음 확인 시각, 프로필, ID)` 다음이 이번 쪽이고, 쪽이 차지 않으면 한 바퀴를 마친 것이라 다음에는 처음부터 다시 본다.
    /// 처리하지 못해 남겨 둔 예약(등록부 장애로 보류된 목적지 예약 등)은 행을 그대로 두므로(발생 시각 · 커서 보존) 다음 바퀴에도 앞쪽에 있지만, 읽기가 그 뒤로 넘어가 다른 예약을 막지 않는다.
    /// 한 바퀴에서 예약마다 **한 번씩만** 차례를 준다(`Pass::served`): 발화로 다음 확인 시각이 앞으로 간 예약이 같은 바퀴의 뒤쪽 쪽에 다시 나타나도 건너뛴다.
    /// 저장소 오류로 중간에 빠져나가면 자리와 차례 기록은 그대로라(처리를 마친 행만 기록돼 있다) 다음 점검이 같은 쪽을 다시 읽어 처리하지 못한 행과 그 뒤 행을 다시 시도한다.
    /// 바퀴 안에서 «발화하고도 남은 발생이 있다»를 쌓아 두었다가 바퀴가 끝나는 점검에서 `backlog`로 한 번 전한다(밀린 발생을 쉬지 않고 이어 따라잡는다).
    pub async fn tick_users(&self) -> Result<UserTick, Error> {
        let limit = self.cfg.batch.clamp(1, 500);
        let after = self
            .user_pass
            .lock()
            .map_or(None, |pass| pass.after.clone());
        let page = self.deps.user.due(after, limit as u32).await?;
        let mut tick = UserTick::default();
        // 정상 행과 손상 행이 섞인 쪽은 저장소가 읽은 순서대로 처리한다
        for row in in_page_order(&page) {
            match row {
                Row::Broken(u) => self.close_unreadable(u, page.now, &mut tick).await?,
                Row::Ready(s) => self.step(s, page.now, &mut tick).await?,
            }
        }
        // 이어 읽을 자리를 기억한다. 쪽이 가득 찼으면 곧바로 다음 쪽을 점검하고(진전이 없었어도 — 같은 묶음을 다시 읽는 것이 아니라 그 뒤를 읽는다),
        // 아니면 한 바퀴를 마쳤으니 남은 발생을 한 번 전하고 차례 기록을 비운다(다음에는 처음부터 다시 본다). 저장소 오류로 위에서 빠져나가면 바꾸지 않아 같은 쪽을 다시 읽는다.
        let mut backlog = page.next.is_some();
        if let Ok(mut pass) = self.user_pass.lock() {
            pass.after.clone_from(&page.next);
            if page.next.is_none() {
                backlog |= std::mem::take(&mut pass.more_due);
                pass.served.clear();
            }
        }
        tick.backlog = backlog;
        Ok(tick)
    }

    /// 저장된 값(cron · 목적지)을 읽지 못한 행 하나를 닫는다: 활성 프로필이면 읽은 커서와 현재 판본으로 저장소가 알려 준 사유로, 프로필이 활성이 아니면 비활성 종료로.
    async fn close_unreadable(
        &self,
        u: &UnreadableSchedule,
        now: SystemTime,
        tick: &mut UserTick,
    ) -> Result<(), Error> {
        if self.served(&u.profile, &u.id) {
            return Ok(());
        }
        let (checked_revision, reason) = match self.deps.catalog.current(&u.profile) {
            Some(snapshot) => (Some(snapshot.revision.clone()), u.reason),
            None => (None, CloseReason::ProfileInactive),
        };
        let command = AdvanceSchedule {
            profile: u.profile.clone(),
            id: u.id.clone(),
            expected_cursor: u.cursor,
            checked_revision,
            next: NextOccurrence::Close(reason),
        };
        self.advance_one(command, now, tick).await?;
        self.serve(&u.profile, &u.id);
        Ok(())
    }

    /// 정상 예약 하나의 차례: 지금 판본으로 다시 보고(`plan`), 목적지를 확인하고, 다음 일을 저장소에 쓴다.
    async fn step(
        &self,
        s: &DueSchedule,
        now: SystemTime,
        tick: &mut UserTick,
    ) -> Result<(), Error> {
        if self.served(&s.profile, &s.id) {
            return Ok(());
        }
        let snapshot = self.deps.catalog.current(&s.profile);
        let (checked_revision, mut next) = plan(s, snapshot.as_deref(), now);
        // 발화할 때마다 목적지를 다시 확인한다: 없어졌거나 팀이 다르면 닫고, 확인하지 못하면 이번 점검에서는 건너뛴다(발화 0)
        if let (NextOccurrence::Fire { .. }, Some(target)) = (&next, &s.spec.destination) {
            match self.route(&s.profile, target) {
                Route::Same => {}
                Route::Gone => next = NextOccurrence::Close(CloseReason::Unrouted),
                Route::Unsure => {
                    self.report_user(&s.profile.0, &s.id.0);
                    self.serve(&s.profile, &s.id); // 이번 바퀴의 차례는 썼다: 다음 바퀴에 다시 본다
                    return Ok(());
                }
            }
        }
        let command = AdvanceSchedule {
            profile: s.profile.clone(),
            id: s.id.clone(),
            expected_cursor: s.cursor,
            checked_revision,
            next,
        };
        if self.advance_one(command, now, tick).await? {
            self.note_more_due();
        }
        self.serve(&s.profile, &s.id);
        Ok(())
    }

    /// 한 예약의 다음 일을 저장소에 쓰고 결과를 집계한다. 발화했고 지금 확인할 발생이 더 남았으면 true. 저장소가 응답하지 못하면 오류.
    async fn advance_one(
        &self,
        command: AdvanceSchedule,
        now: SystemTime,
        tick: &mut UserTick,
    ) -> Result<bool, Error> {
        let (closes, waits) = (
            matches!(command.next, NextOccurrence::Close(_)),
            matches!(command.next, NextOccurrence::WaitUntil(_)),
        );
        let remaining =
            matches!(&command.next, NextOccurrence::Fire { next: Some(next), .. } if *next <= now);
        let (profile, id) = (command.profile.0.clone(), command.id.0.clone());
        match self.deps.user.advance(command).await {
            Ok(Some(_)) => {
                tick.fired += 1;
                return Ok(remaining);
            }
            Ok(None) if closes => tick.closed += 1,
            Ok(None) if waits => tick.waiting += 1,
            Ok(None) => {}
            // 다른 스케줄러가 먼저 움직였거나 판본이 그 사이 바뀌었다: 다음 바퀴에서 다시 본다
            Err(Error::Stale) => {}
            // 저장소가 이 입력을 받지 않는다: 한 예약 때문에 나머지를 막지 않는다(한 번만 기록)
            Err(Error::Denied | Error::Invalid | Error::Missing | Error::Conflict) => {
                self.report_user(&profile, &id);
            }
            Err(error) => return Err(error),
        }
        Ok(false)
    }

    /// 이번 바퀴에서 이미 차례를 준 예약인가.
    fn served(&self, profile: &ProfileId, id: &ScheduleId) -> bool {
        self.user_pass
            .lock()
            .is_ok_and(|pass| pass.served.contains(&(profile.clone(), id.clone())))
    }

    /// 이번 바퀴에 «발화하고도 남은 발생이 있다»를 쌓는다(바퀴가 끝나는 점검이 한 번 전하고 비운다).
    fn note_more_due(&self) {
        if let Ok(mut pass) = self.user_pass.lock() {
            pass.more_due = true;
        }
    }

    /// 이번 바퀴의 차례를 줬다고 기록한다(처리를 마친 뒤에만 — 저장소 오류로 중단된 행은 다음 점검이 다시 시도한다).
    fn serve(&self, profile: &ProfileId, id: &ScheduleId) {
        if let Ok(mut pass) = self.user_pass.lock() {
            pass.served.insert((profile.clone(), id.clone()));
        }
    }

    fn report_user(&self, profile: &str, id: &str) {
        let key = (profile.to_owned(), "user".to_owned(), id.to_owned(), 0);
        let first = self
            .reported
            .lock()
            .map(|mut seen| seen.insert(key))
            .unwrap_or(false);
        if first {
            tracing::warn!(
                code = "user_schedule_refused",
                "scheduler skipped a user schedule"
            );
        }
    }
}
