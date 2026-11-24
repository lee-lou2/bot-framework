//! 스케줄러: 저장소의 구간·커서를 읽어 놓친 발생까지 접수한다. cron 계산은 여기서 하고(`cron.rs`), 저장소는 구간·커서·중복 방지·beat 합치기를 맡는다.
//!
//! - 시각은 스냅샷의 DB 시각(`ScheduleSnapshot::now`)만 쓴다. 호스트 시계는 판정에 쓰지 않는다.
//! - 트리거마다 (커서, 지금] 안의 발생을 구간(`from`, `until`]별로 오래된 순서로 접수한다. 한 번에 `batch`개까지 따라잡고 남으면 곧바로 다시 돈다.
//! - beat(상태 확인용)는 놓친 발생이 여럿이어도 **가장 최근 하나만** 접수한다(커서가 그 시각으로 건너뛴다). 그 밖의 트리거는 하나도 빼지 않는다.
//! - 두 스케줄러가 같은 발생을 접수해도 저장소가 같은 (profile, agent, trigger, 시각)을 한 run으로 합친다. 요청 ID는 `cron:<트리거>:<UNIX ms>`다.
//! - 해석할 수 없는 cron·시간대의 구간은 건너뛴다(고정 코드로 한 번만 기록한다). 다른 트리거는 계속 돈다.

mod user;

pub use user::UserTick;

use crate::{
    cron::Cron,
    host::{SchedulerConfig, SchedulerDeps},
    scope,
};
use bot_core::{
    command::{
        Accept, Envelope, JsonPayload, Occurrence, ScheduleSet, ScheduledTrigger, Segment, Target,
        TriggerSpec,
    },
    id::{BindingId, RequestId},
    port::Error,
    profile::{Snapshot, TriggerSource, TriggerTarget},
    state::{Conversation, ConversationKind},
};
use chrono_tz::Tz;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

/// 예약 발생의 바인딩. 외부 입구가 아니라 스케줄러가 접수한 것임을 나타낸다.
const CRON_BINDING: &str = "@cron";

/// 한 번의 점검 결과.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tick {
    /// 이번에 접수한(또는 이미 접수돼 있던) 발생 수
    pub accepted: usize,
    /// 한도(`batch`) 때문에 남은 발생이 있다
    pub backlog: bool,
    /// 해석할 수 없어 건너뛴 구간 수
    pub skipped: usize,
}

/// 이미 기록한 해석 불가 구간의 키: (프로필, 에이전트, 트리거, 구간 시작 UNIX ms)
type SegmentKey = (String, String, String, i64);

#[derive(Clone)]
pub struct Scheduler {
    deps: Arc<SchedulerDeps>,
    cfg: Arc<SchedulerConfig>,
    reported: Arc<Mutex<HashSet<SegmentKey>>>,
    /// 명시 목적지(다른 수신자)를 발화 전에 현재 운영자 등록부로 다시 확인하는 입구. 없으면 목적지가 있는 예약은 발화하지도 닫지도 않는다.
    routes: Option<Arc<dyn bot_core::schedule::ScheduleRoutes>>,
    /// 사용자 예약을 한 바퀴 동안 이어 읽는 상태(이어 읽을 자리 · 이번 바퀴에서 이미 차례를 준 예약).
    user_pass: Arc<Mutex<user::Pass>>,
}

/// 판본의 cron 트리거 전체를 저장소가 받는 모양으로 옮긴다(활성 집합 게시용).
/// 실행 범위는 트리거 주체의 역할 범위 ∩ 에이전트·프로필·정책 상한이다(멤버가 아니면 비어 있다).
pub fn schedule_set(snapshot: &Snapshot) -> ScheduleSet {
    let mut triggers = Vec::new();
    for (agent_id, agent) in &snapshot.agents {
        for trigger in &agent.triggers {
            let TriggerSource::Cron {
                schedule,
                beat,
                principal,
                approver,
            } = &trigger.on
            else {
                continue;
            };
            let target = match &trigger.target {
                TriggerTarget::Instruction(text) => Target::Agent {
                    agent: agent_id.clone(),
                    instruction: text.clone(),
                },
                TriggerTarget::Tool { tool, args } => Target::Tool {
                    agent: agent_id.clone(),
                    tool: tool.clone(),
                    args: args.clone(),
                },
            };
            triggers.push(TriggerSpec {
                approver: approver.clone(),
                trigger: trigger.id.clone(),
                cron: schedule.clone(),
                timezone: snapshot.timezone.clone(),
                beat: *beat,
                principal: principal.clone(),
                scopes: scope::effective(snapshot, principal, agent_id).unwrap_or_default(),
                target,
            });
        }
    }
    ScheduleSet {
        profile: snapshot.id.clone(),
        revision: snapshot.revision.clone(),
        triggers,
    }
}

fn unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// 접수 명령. 구간의 저장된 정의를 그대로 옮긴다(저장소가 정의와 같은지 다시 대조한다).
fn accept_for(trigger: &ScheduledTrigger, segment: &Segment, at: SystemTime) -> Accept {
    Accept {
        event: Envelope {
            profile: trigger.profile.clone(),
            request: RequestId(format!("cron:{}:{}", trigger.trigger, unix_ms(at))),
            binding: BindingId(CRON_BINDING.into()),
            principal: segment.spec.principal.clone(),
            conversation: Conversation {
                kind: ConversationKind::Scheduled,
                id: trigger.trigger.clone(),
                thread: None,
            },
            http: None,
            attachments: Vec::new(),
            body: JsonPayload("{}".into()),
        },
        approver: segment.spec.approver.clone(),
        revision: segment.revision.clone(),
        target: segment.spec.target.clone(),
        scopes: segment.spec.scopes.clone(),
    }
}

impl Scheduler {
    pub fn new(deps: SchedulerDeps, cfg: SchedulerConfig) -> Self {
        Self {
            deps: Arc::new(deps),
            cfg: Arc::new(cfg),
            reported: Arc::new(Mutex::new(HashSet::new())),
            routes: None,
            user_pass: Arc::new(Mutex::new(user::Pass::default())),
        }
    }

    /// 명시 목적지 예약의 발화 전 확인에 쓸 등록부를 붙인다(호스트가 봉인에 쓴 것과 같은 것을 준다).
    #[must_use]
    pub fn with_routes(mut self, routes: Arc<dyn bot_core::schedule::ScheduleRoutes>) -> Self {
        self.routes = Some(routes);
        self
    }

    /// 한 번 점검한다: 스냅샷을 읽고 접수할 발생을 접수한다. 저장소가 응답하지 못하면 오류(다음 점검에서 이어 간다).
    pub async fn tick(&self) -> Result<Tick, Error> {
        let snapshot = self.deps.schedule.snapshot().await?;
        let mut tick = Tick::default();
        for trigger in &snapshot.triggers {
            self.follow(trigger, snapshot.now, &mut tick).await?;
        }
        Ok(tick)
    }

    /// 종료 신호(`true`)가 올 때까지 점검을 반복한다. 남은 발생이 있으면 쉬지 않고 이어서 돈다.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                break;
            }
            let wait = match self.tick().await {
                Ok(tick) if tick.backlog => Duration::ZERO,
                Ok(_) => self.cfg.interval,
                // 옛 활성 집합에 묶인 스케줄러는 더 돌지 않는다(교체된 쪽이 이어 간다)
                Err(Error::Stale) => {
                    tracing::warn!(code = "scheduler_stale", "scheduler stopped: replaced set");
                    break;
                }
                Err(_) => {
                    tracing::warn!(code = "snapshot_unavailable", "scheduler tick failed");
                    self.cfg.interval
                }
            };
            // 사용자 예약은 정적 트리거와 따로 본다(한쪽이 막혀도 다른 쪽은 돈다)
            let wait = match self.tick_users().await {
                Ok(tick) if tick.backlog => Duration::ZERO,
                Ok(_) => wait,
                Err(Error::Stale) => {
                    tracing::warn!(code = "scheduler_stale", "scheduler stopped: replaced set");
                    break;
                }
                Err(_) => {
                    tracing::warn!(
                        code = "user_schedules_unavailable",
                        "scheduler user tick failed"
                    );
                    wait.min(self.cfg.interval)
                }
            };
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    async fn follow(
        &self,
        trigger: &ScheduledTrigger,
        now: SystemTime,
        tick: &mut Tick,
    ) -> Result<(), Error> {
        let mut budget = self.cfg.batch.max(1);
        for segment in &trigger.segments {
            let lo = trigger.cursor.max(segment.from);
            let hi = segment.until.map_or(now, |until| until.min(now));
            if hi <= lo {
                continue;
            }
            let Some((cron, zone)) = self.parse(trigger, segment) else {
                tick.skipped += 1;
                continue;
            };
            let due = if segment.spec.beat {
                // 놓친 발생이 여럿이어도 가장 최근 하나만(커서가 그 시각으로 건너뛴다). 구간 끝에서 직접 찾는다:
                // 아무리 오래 밀렸어도 오래된 순으로 세다 멈춘 것이 아니라 정말 마지막 발생이다
                cron.last_in(zone, lo, hi).into_iter().collect()
            } else {
                cron.occurrences(zone, lo, hi, budget + 1)
            };
            let more = due.len() > budget;
            for at in due.into_iter().take(budget) {
                let occurrence = Occurrence {
                    profile: trigger.profile.clone(),
                    agent: trigger.agent.clone(),
                    trigger: trigger.trigger.clone(),
                    scheduled_at: at,
                };
                match self
                    .deps
                    .schedule
                    .accept_occurrence(occurrence, accept_for(trigger, segment, at))
                    .await
                {
                    Ok(_) => {
                        tick.accepted += 1;
                        budget -= 1;
                    }
                    // 다른 스케줄러가 더 뒤의 발생까지 접수해 커서가 지나갔다
                    Err(Error::Stale) => {}
                    // 정의가 그 사이 바뀌었다: 이 트리거는 다음 스냅샷에서 다시 본다
                    Err(Error::Denied | Error::Invalid | Error::Missing) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            if more {
                tick.backlog = true;
                return Ok(()); // 커서는 순서대로만 나아간다: 뒤 구간은 다음 점검에서
            }
            if budget == 0 {
                tick.backlog = true;
                return Ok(());
            }
        }
        Ok(())
    }

    /// 구간의 cron·시간대를 해석한다. 못 하면 구간을 건너뛰고 고정 코드로 한 번만 기록한다.
    fn parse(&self, trigger: &ScheduledTrigger, segment: &Segment) -> Option<(Cron, Tz)> {
        let parsed = Cron::parse(&segment.spec.cron)
            .ok()
            .zip(segment.spec.timezone.parse::<Tz>().ok());
        if parsed.is_none() {
            let key = (
                trigger.profile.0.clone(),
                trigger.agent.0.clone(),
                trigger.trigger.clone(),
                unix_ms(segment.from),
            );
            let first = self
                .reported
                .lock()
                .map(|mut seen| seen.insert(key))
                .unwrap_or(false);
            if first {
                tracing::warn!(
                    code = "schedule_unparsable",
                    trigger = %trigger.trigger,
                    "scheduler skipped a segment"
                );
            }
        }
        parsed
    }
}

#[cfg(test)]
mod page_tests;
#[cfg(test)]
mod route_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod turn_tests;
#[cfg(test)]
mod user_tests;
