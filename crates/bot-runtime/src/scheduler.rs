//! Cron and user-schedule loop that computes due occurrences against store time and admits runs idempotently.

mod user;

pub use user::UserTick;

use crate::{
    cron::Cron,
    host::{SchedulerConfig, SchedulerDeps},
};
use bot_core::{
    command::{
        Accept, Envelope, JsonPayload, Occurrence, ScheduleSet, ScheduledTrigger, Segment, Target,
        TriggerSpec,
    },
    id::{BindingId, RequestId},
    port::Error,
    profile::{Snapshot, TriggerSource, TriggerTarget},
    scope,
    state::{Conversation, ConversationKind},
};
use chrono_tz::Tz;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

const CRON_BINDING: &str = "@cron";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tick {
    pub accepted: usize,
    pub backlog: bool,
    pub skipped: usize,
}

type SegmentKey = (String, String, String, i64);

#[derive(Clone)]
pub struct Scheduler {
    deps: Arc<SchedulerDeps>,
    cfg: Arc<SchedulerConfig>,
    reported: Arc<Mutex<HashSet<SegmentKey>>>,
    /// Re-verifies explicit-destination user schedules against operator routes before firing; if absent, such schedules wait.
    routes: Option<Arc<dyn bot_core::schedule::ScheduleRoutes>>,
    user_pass: Arc<Mutex<user::Pass>>,
}

/// Builds the active-set `ScheduleSet` from cron triggers in `snapshot` using effective principal/agent/policy scopes.
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

fn accept_for(trigger: &ScheduledTrigger, segment: &Segment, at: SystemTime) -> Accept {
    Accept {
        reply_transport: None,
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

    #[must_use]
    pub fn with_routes(mut self, routes: Arc<dyn bot_core::schedule::ScheduleRoutes>) -> Self {
        self.routes = Some(routes);
        self
    }

    pub async fn tick(&self) -> Result<Tick, Error> {
        let snapshot = self.deps.schedule.snapshot().await?;
        let mut tick = Tick::default();
        for trigger in &snapshot.triggers {
            self.follow(trigger, snapshot.now, &mut tick).await?;
        }
        Ok(tick)
    }

    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                break;
            }
            let wait = match self.tick().await {
                Ok(tick) if tick.backlog => Duration::ZERO,
                Ok(_) => self.cfg.interval,
                Err(Error::Stale) => {
                    tracing::warn!(code = "scheduler_stale", "scheduler stopped: replaced set");
                    break;
                }
                Err(_) => {
                    tracing::warn!(code = "snapshot_unavailable", "scheduler tick failed");
                    self.cfg.interval
                }
            };
            // User schedules tick independently so trouble in one path does not stall the other.
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
                // Beats coalesce missed occurrences by walking backwards from `hi` to find the true latest match.
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
                    Err(Error::Stale) => {}
                    Err(Error::Denied | Error::Invalid | Error::Missing) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            if more || budget == 0 {
                tick.backlog = true;
                return Ok(());
            }
        }
        Ok(())
    }

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
