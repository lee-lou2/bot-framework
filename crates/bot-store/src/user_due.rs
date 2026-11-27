//! User schedule firing: keyset-paginated `due` scans and atomic `advance` (fire, wait, or close).
//!
//! Lock order: active revision -> (origin run, when posting a close notice) -> schedule row -> audit head.

use crate::{
    accept::{check, insert_run},
    audit::{self, OwnedEntry},
    codec::*,
    db::{R, Store, Tx, ms_to_time, time_to_ms},
    user_schedule::{destination_target, origin_target},
};
use bot_core::{
    command::{Accept, Envelope, JsonPayload, Target},
    cron::Cron,
    id::*,
    intake::PushTransportId,
    port::Error,
    schedule::*,
    state::Conversation,
};
use serde_json::json;
use sqlx::{Row, postgres::PgRow};
use std::collections::BTreeSet;

fn pos_of(row: &PgRow) -> DuePos {
    DuePos {
        at: ms_to_time(row.get("next_ms")),
        profile: ProfileId(row.get("profile")),
        id: ScheduleId(row.get("id")),
    }
}

/// Parses stored timing; returns `None` on corrupt/unparseable rows so callers can close just that schedule.
fn timing_of(row: &PgRow) -> Option<Timing> {
    match (
        row.get::<Option<i64>, _>("at_ms"),
        row.get::<Option<String>, _>("cron"),
    ) {
        (Some(at), None) => Some(Timing::At(ms_to_time(at))),
        (None, Some(cron)) => Some(Timing::Cron {
            expression: Cron::parse(&cron).ok()?,
            timezone: row.get("timezone"),
        }),
        _ => None,
    }
}

/// Decodes a schedule row, returning `Ok(Err(reason))` for corrupt data (timing checked before destination).
fn due_of(row: &PgRow) -> R<Result<DueSchedule, CloseReason>> {
    let Some(timing) = timing_of(row) else {
        return Ok(Err(CloseReason::InvalidTiming));
    };
    let Ok(destination) = destination_target(row) else {
        return Ok(Err(CloseReason::InvalidDestination));
    };
    let ceiling: Vec<String> = row.get("ceiling");
    Ok(Ok(DueSchedule {
        profile: ProfileId(row.get("profile")),
        id: ScheduleId(row.get("id")),
        owner: PrincipalId(row.get("owner")),
        agent: AgentId(row.get("agent")),
        origin: origin_target(row)?,
        ceiling: ceiling.into_iter().collect(),
        spec: ScheduleSpec {
            timing,
            instruction: row.get("instruction"),
            quiet: row.get("quiet"),
            destination,
        },
        cursor: ms_to_time(row.get("cursor_ms")),
        at: ms_to_time(row.get("next_ms")),
    }))
}

/// Builds the run's `Accept` command strictly from stored schedule and origin fields.
fn accept_of(
    row: &PgRow,
    revision: &Revision,
    at_ms: i64,
    scopes: &BTreeSet<String>,
    reply_transport: Option<PushTransportId>,
) -> R<Accept> {
    let id: String = row.get("id");
    let agent = AgentId(row.get("agent"));
    let instruction: String = row.get("instruction");
    Ok(Accept {
        reply_transport,
        event: Envelope {
            profile: ProfileId(row.get("profile")),
            request: RequestId(format!("usr:{id}:{at_ms}")),
            binding: BindingId(row.get("origin_binding")),
            principal: PrincipalId(row.get("owner")),
            conversation: Conversation {
                kind: kind_from(&row.get::<String, _>("origin_kind"))?,
                id: row.get("origin_id"),
                thread: row.get("origin_thread"),
            },
            http: None,
            attachments: Vec::new(),
            body: JsonPayload(json!({ "text": instruction }).to_string()),
        },
        approver: row.get::<Option<String>, _>("approver").map(PrincipalId),
        revision: revision.clone(),
        target: Target::Agent { agent, instruction },
        scopes: scopes.clone(),
    })
}

/// Verifies the active profile revision, or confirms profile inactivity for `Close(ProfileInactive)` under `FOR SHARE`.
async fn guard(tx: &mut Tx<'_>, cmd: &AdvanceSchedule) -> R<()> {
    match &cmd.checked_revision {
        Some(_)
            if matches!(
                cmd.next,
                NextOccurrence::Close(CloseReason::ProfileInactive)
            ) =>
        {
            invalid()
        }
        Some(rev) => crate::active::guard_active(tx, &cmd.profile.0, rev).await,
        None if matches!(
            cmd.next,
            NextOccurrence::Close(CloseReason::ProfileInactive)
        ) =>
        {
            sqlx::query("select 1 from bot_active where singleton for share")
                .fetch_optional(&mut **tx)
                .await?;
            let active: Option<String> =
                sqlx::query_scalar("select revision from bot_active_profiles where profile = $1")
                    .bind(&cmd.profile.0)
                    .fetch_optional(&mut **tx)
                    .await?;
            if active.is_some() {
                return Err(Error::Stale.into());
            }
            Ok(())
        }
        None => invalid(),
    }
}

impl Store {
    /// Keyset-scans due active schedules in `(next_ms, profile, id)` order within a frozen sweep upper bound `end`.
    /// Unreadable rows are returned separately in `unreadable` without blocking the page or mutating state.
    pub(crate) async fn schedule_due(&self, after: Option<DueKey>, limit: u32) -> R<DuePage> {
        let limit = limit.clamp(1, 500);
        let mut tx = self.begin_authorized().await?;
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let end = match &after {
            Some(key) => key.end.clone(),
            None => {
                let last = sqlx::query(
                    "select next_ms, profile, id from bot_user_schedules where state = 'active' and next_ms <= $1
                     order by next_ms desc, profile desc, id desc limit 1",
                )
                .bind(now)
                .fetch_optional(&mut *tx)
                .await?;
                let Some(last) = last else {
                    tx.rollback().await?;
                    return Ok(DuePage {
                        now: ms_to_time(now),
                        schedules: Vec::new(),
                        unreadable: Vec::new(),
                        next: None,
                    });
                };
                pos_of(&last)
            }
        };
        let from = if after.is_some() {
            "and (next_ms, profile, id) > ($6, $7, $8)"
        } else {
            ""
        };
        let sql = format!(
            "select {} from bot_user_schedules where state = 'active' and next_ms <= $1
               and (next_ms, profile, id) <= ($3, $4, $5) {from} order by next_ms, profile, id limit $2", crate::cols::USER_SCHEDULES
        );
        // Fetch limit + 1 to detect whether more rows remain within this sweep bound.
        let mut query = sqlx::query(&sql)
            .bind(now)
            .bind(i64::from(limit) + 1)
            .bind(time_to_ms(end.at))
            .bind(&end.profile.0)
            .bind(&end.id.0);
        if let Some(key) = &after {
            query = query
                .bind(time_to_ms(key.after.at))
                .bind(&key.after.profile.0)
                .bind(&key.after.id.0);
        }
        let mut rows = query.fetch_all(&mut *tx).await?;
        tx.rollback().await?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next = rows.last().filter(|_| more).map(|last| DueKey {
            after: pos_of(last),
            end,
        });
        let (mut schedules, mut unreadable) = (Vec::new(), Vec::new());
        for row in &rows {
            match due_of(row)? {
                Ok(ready) => schedules.push(ready),
                Err(reason) => unreadable.push(UnreadableSchedule {
                    profile: ProfileId(row.get("profile")),
                    id: ScheduleId(row.get("id")),
                    cursor: ms_to_time(row.get("cursor_ms")),
                    at: ms_to_time(row.get("next_ms")),
                    reason,
                }),
            }
        }
        Ok(DuePage {
            now: ms_to_time(now),
            schedules,
            unreadable,
            next,
        })
    }

    pub(crate) async fn schedule_advance(&self, cmd: AdvanceSchedule) -> R<Option<RunId>> {
        let (profile, id) = (&cmd.profile.0, &cmd.id.0);
        ids_ok(&[profile, id])?;
        let expected = time_to_ms(cmd.expected_cursor);
        let mut tx = self.begin_authorized().await?;
        guard(&mut tx, &cmd).await?;
        // Lock the origin run before the schedule row when a close notice will be enqueued (preserves run -> schedule order).
        if let NextOccurrence::Close(closing) = &cmd.next
            && closing.notice().is_some()
        {
            crate::user_notice::lock_origin_run(&mut tx, profile, id).await?;
        }
        let row = sqlx::query(&format!(
            "select {} from bot_user_schedules where profile = $1 and id = $2 for update",
            crate::cols::USER_SCHEDULES
        ))
        .bind(profile)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let state: String = row.get("state");
        let cursor: i64 = row.get("cursor_ms");
        // Closed schedules ignore further closes and converge matching `Fire` retries onto the already-accepted run.
        if state != "active" {
            tx.rollback().await?;
            return match cmd.next {
                NextOccurrence::Close(_) => Ok(None),
                NextOccurrence::Fire { at, .. } => self
                    .occurrence_run(profile, id, time_to_ms(at))
                    .await?
                    .map(Some)
                    .ok_or(Error::Stale.into()),
                NextOccurrence::WaitUntil(_) => Err(Error::Stale.into()),
            };
        }
        match cmd.next {
            NextOccurrence::Close(closing) => {
                // Reject stale close decisions if another scheduler already moved the cursor.
                if cursor != expected {
                    return Err(Error::Stale.into());
                }
                let reason = closing.code();
                close(&mut tx, profile, id, reason).await?;
                let mut audits = Vec::new();
                if let Some(text) = closing.notice() {
                    crate::user_notice::push_into(&mut tx, &row, text, &mut audits).await?;
                }
                audits.push(OwnedEntry::new(
                    profile,
                    "system",
                    "schedule.close",
                    id,
                    reason,
                ));
                audit::append_many(&mut tx, &audits).await?;
                tx.commit().await?;
                Ok(None)
            }
            NextOccurrence::WaitUntil(until) => {
                let until = time_to_ms(until);
                if cursor != expected {
                    return Err(Error::Stale.into());
                }
                if until <= cursor {
                    return invalid();
                }
                sqlx::query(
                    "update bot_user_schedules set next_ms = $3, updated_ms = bot_now_ms() where profile = $1 and id = $2",
                )
                .bind(profile)
                .bind(id)
                .bind(until)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(None)
            }
            NextOccurrence::Fire { at, next, scopes } => {
                let (at_ms, next_ms) = (time_to_ms(at), next.map(time_to_ms));
                if cursor != expected {
                    // Another scheduler moved the cursor: converge if the same occurrence was accepted, otherwise reject as stale.
                    tx.rollback().await?;
                    return self
                        .occurrence_run(profile, id, at_ms)
                        .await?
                        .map(Some)
                        .ok_or(Error::Stale.into());
                }
                let now: i64 = sqlx::query_scalar("select bot_now_ms()")
                    .fetch_one(&mut *tx)
                    .await?;
                let ceiling: Vec<String> = row.get("ceiling");
                let timed_at: Option<i64> = row.get("at_ms");
                // Reject future times (host clock ahead), backward cursors, mismatched one-shot times, or scopes exceeding the ceiling.
                if at_ms > now
                    || at_ms <= cursor
                    || next_ms.is_some_and(|n| n <= at_ms)
                    || timed_at.is_some_and(|t| t != at_ms || next_ms.is_some())
                    || !scopes.iter().all(|s| ceiling.contains(s))
                {
                    return invalid();
                }
                let revision = cmd.checked_revision.clone().ok_or(Error::Invalid)?;
                // Reject firing without closing if a legacy push origin is missing `reply_transport`,
                // so operators can backfill the mapping without losing the user's schedule.
                let origin_transport: Option<String> = sqlx::query_scalar(
                    "select reply_transport from bot_runs where profile = $1 and id = $2",
                )
                .bind(profile)
                .bind(row.get::<String, _>("origin_run"))
                .fetch_one(&mut *tx)
                .await?;
                let origin_kind: String = row.get("origin_kind");
                if matches!(origin_kind.as_str(), "direct" | "channel")
                    && origin_transport.is_none()
                {
                    return invalid();
                }
                let reply_transport = origin_transport.map(PushTransportId::new).transpose()?;
                let accept = accept_of(&row, &revision, at_ms, &scopes, reply_transport)?;
                let checked = check(&accept)?;
                let run: String = sqlx::query_scalar("select gen_random_uuid()::text")
                    .fetch_one(&mut *tx)
                    .await?;
                insert_run(
                    &mut tx,
                    &accept,
                    &checked,
                    &run,
                    bot_core::state::RunOrigin::from_recorded(row.get("fire_origin")),
                    Some(&format!("user:{id}")),
                    None,
                )
                .await?;
                sqlx::query(
                    "update bot_runs set quiet = $3, wake_depth = $4, private_memory_read = $5 where profile = $1 and id = $2",
                )
                .bind(profile)
                .bind(&run)
                .bind(row.get::<bool, _>("quiet"))
                .bind(row.get::<i16, _>("wake_depth"))
                .bind(row.get::<bool, _>("origin_private_read"))
                .execute(&mut *tx)
                .await?;
                // Pin explicit destination to this run at fire time so later schedule cancellation does not alter it.
                if row.get::<Option<String>, _>("dest_binding").is_some() {
                    sqlx::query(
                        "insert into bot_deliveries (profile, run, dest_binding, dest_kind, dest_id, dest_thread)
                         select $1, $3, dest_binding, dest_kind, dest_id, dest_thread from bot_user_schedules
                         where profile = $1 and id = $2",
                    )
                    .bind(profile)
                    .bind(id)
                    .bind(&run)
                    .execute(&mut *tx)
                    .await?;
                }
                sqlx::query(
                    "insert into bot_user_occurrences (profile, schedule, at_ms, run) values ($1, $2, $3, $4)",
                )
                .bind(profile)
                .bind(id)
                .bind(at_ms)
                .bind(&run)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "update bot_user_schedules
                     set cursor_ms = $3, next_ms = $4, state = case when $4::bigint is null then 'closed' else state end,
                         close_reason = case when $4::bigint is null then $5 else close_reason end, updated_ms = bot_now_ms()
                     where profile = $1 and id = $2",
                )
                .bind(profile)
                .bind(id)
                .bind(at_ms)
                .bind(next_ms)
                .bind(CloseReason::Finished.code())
                .execute(&mut *tx)
                .await?;
                let mut audits = vec![OwnedEntry::new(
                    profile,
                    &accept.event.principal.0,
                    "schedule.fire",
                    &run,
                    "accepted",
                )];
                // Final occurrence closes the schedule as `Finished` in the same commit.
                if next_ms.is_none() {
                    audits.push(OwnedEntry::new(
                        profile,
                        "system",
                        "schedule.close",
                        id,
                        CloseReason::Finished.code(),
                    ));
                }
                audit::append_many(&mut tx, &audits).await?;
                tx.commit().await?;
                Ok(Some(RunId(run)))
            }
        }
    }

    async fn occurrence_run(&self, profile: &str, schedule: &str, at_ms: i64) -> R<Option<RunId>> {
        let run: Option<String> = sqlx::query_scalar(
            "select run from bot_user_occurrences where profile = $1 and schedule = $2 and at_ms = $3",
        )
        .bind(profile)
        .bind(schedule)
        .bind(at_ms)
        .fetch_optional(&self.pool)
        .await?;
        Ok(run.map(RunId))
    }
}

async fn close(tx: &mut Tx<'_>, profile: &str, id: &str, reason: &str) -> R<()> {
    sqlx::query(
        "update bot_user_schedules set state = 'closed', close_reason = $3, next_ms = null, updated_ms = bot_now_ms()
         where profile = $1 and id = $2",
    )
    .bind(profile)
    .bind(id)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
