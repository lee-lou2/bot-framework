//! Chat-created user schedule firing with live-revision re-evaluation and fair keyset pagination.
//!
//! Each pass serves every schedule at most once (`Pass::served`) so backlogged recurring schedules
//! advancing into later pages cannot starve others, while carrying `more_due` to the end of the
//! pass to trigger an immediate follow-up pass.

use super::Scheduler;
use crate::cron::Cron;
use bot_core::{
    id::{ProfileId, Revision, ScheduleId},
    port::Error,
    profile::{Scopes, Snapshot},
    schedule::{
        AdvanceSchedule, CloseReason, DeliveryTarget, DueKey, DuePage, DueSchedule, NextOccurrence,
        Timing, UnreadableSchedule,
    },
    scope,
};
use chrono_tz::Tz;
use std::{collections::HashSet, time::SystemTime};

#[derive(Default)]
pub(super) struct Pass {
    after: Option<DueKey>,
    served: HashSet<(ProfileId, ScheduleId)>,
    more_due: bool,
}

enum Row<'a> {
    Ready(&'a DueSchedule),
    Broken(&'a UnreadableSchedule),
}

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UserTick {
    pub fired: usize,
    pub waiting: usize,
    pub closed: usize,
    pub backlog: bool,
}

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

enum Route {
    Same,
    Gone,
    Unsure,
}

fn same_target(a: &DeliveryTarget, b: &DeliveryTarget) -> bool {
    a.binding == b.binding
        && a.conversation.kind == b.conversation.kind
        && a.conversation.id == b.conversation.id
        && a.conversation.thread == b.conversation.thread
}

impl Scheduler {
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

    pub async fn tick_users(&self) -> Result<UserTick, Error> {
        let limit = self.cfg.batch.clamp(1, 500);
        let after = self
            .user_pass
            .lock()
            .map_or(None, |pass| pass.after.clone());
        let page = self.deps.user.due(after, limit as u32).await?;
        let mut tick = UserTick::default();
        // Process valid and unreadable rows in storage order so a locked broken row does not stall earlier rows.
        for row in in_page_order(&page) {
            match row {
                Row::Broken(u) => self.close_unreadable(u, page.now, &mut tick).await?,
                Row::Ready(s) => self.step(s, page.now, &mut tick).await?,
            }
        }
        // Only update the pass cursor after the whole page succeeds so transient store errors retry this page.
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
        // Re-verify explicit destinations before firing: close if unrouted/moved, hold on transient lookup failure.
        if let (NextOccurrence::Fire { .. }, Some(target)) = (&next, &s.spec.destination) {
            match self.route(&s.profile, target) {
                Route::Same => {}
                Route::Gone => next = NextOccurrence::Close(CloseReason::Unrouted),
                Route::Unsure => {
                    self.report_user(&s.profile.0, &s.id.0);
                    self.serve(&s.profile, &s.id);
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
            Err(Error::Stale) => {}
            Err(Error::Denied | Error::Invalid | Error::Missing | Error::Conflict) => {
                self.report_user(&profile, &id);
            }
            Err(error) => return Err(error),
        }
        Ok(false)
    }

    fn served(&self, profile: &ProfileId, id: &ScheduleId) -> bool {
        self.user_pass
            .lock()
            .is_ok_and(|pass| pass.served.contains(&(profile.clone(), id.clone())))
    }

    fn note_more_due(&self) {
        if let Ok(mut pass) = self.user_pass.lock() {
            pass.more_due = true;
        }
    }

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
                profile = %profile,
                schedule = %id,
                "scheduler skipped a user schedule"
            );
        }
    }
}
