//! User schedule listing and atomic tool-triggered follow-up scheduling (`finish_with_wake`, capped at `MAX_WAKE_DEPTH`).

use crate::{
    audit::{self, OwnedEntry},
    calls::{fence, lock_call, lock_root_shared, phase_of},
    codec::*,
    db::{R, Store},
    user_schedule::{
        New, done_entry, insert, instruction_ok, origin_of, summary_of, timed, within_limit,
    },
};
use bot_core::{command::JsonPayload, id::*, ledger::CallPhase, port::Error, schedule::*};
use sqlx::Row;

impl Store {
    pub(crate) async fn schedule_list(
        &self,
        at: &Attempt,
        checked: &Revision,
    ) -> R<Vec<ScheduleSummary>> {
        ids_ok(&[&checked.0])?;
        let (mut tx, me) = crate::memory::begin_read(self, at).await?;
        let active: Option<String> =
            sqlx::query_scalar("select revision from bot_active_profiles where profile = $1")
                .bind(&at.profile.0)
                .fetch_optional(&mut *tx)
                .await?;
        if active.as_deref() != Some(checked.0.as_str()) {
            return Err(Error::Stale.into());
        }
        let rows = sqlx::query(
            "select id, at_ms, cron, timezone, quiet from bot_user_schedules
             where profile = $1 and owner = $2 and state = 'active' order by id limit $3",
        )
        .bind(&at.profile.0)
        .bind(me.get::<String, _>("principal"))
        .bind(MAX_ACTIVE_PER_OWNER as i64)
        .fetch_all(&mut *tx)
        .await?;
        tx.rollback().await?;
        rows.iter().map(summary_of).collect()
    }

    pub(crate) async fn schedule_wake(
        &self,
        at: &Attempt,
        call: &CallId,
        checked: &Revision,
        result: JsonPayload,
        wake: Wake,
    ) -> R<ScheduleId> {
        ids_ok(&[&call.0, &checked.0])?;
        instruction_ok(&wake.instruction)?;
        let (_, result_text) = canonical(&result)?;
        let mut tx = self.begin_authorized().await?;
        crate::active::guard_active(&mut tx, &at.profile.0, checked).await?;
        let shared_flag = lock_root_shared(&mut tx, at).await?;
        let run = fence(&mut tx, at).await?;
        let row = lock_call(&mut tx, at, call).await?;
        match phase_of(&row)? {
            CallPhase::Started { generation, .. } if generation.0 <= at.generation.0 => {}
            CallPhase::Done(_) => {
                // Idempotent replay of a completed wake call returns the schedule it created.
                let existing: Option<String> = sqlx::query_scalar(
                    "select id from bot_user_schedules where profile = $1 and origin_run = $2 and origin_call = $3",
                )
                .bind(&at.profile.0)
                .bind(&at.run.0)
                .bind(&call.0)
                .fetch_optional(&mut *tx)
                .await?;
                tx.rollback().await?;
                return existing.map(ScheduleId).ok_or(Error::Conflict.into());
            }
            _ => return Err(Error::Conflict.into()),
        }
        let origin = origin_of(&mut tx, at, &run, shared_flag).await?;
        let depth = origin.depth + 1;
        if depth > i16::from(MAX_WAKE_DEPTH) {
            return Err(Error::Denied.into());
        }
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let timed = timed(&Timing::At(wake.at), now)?;
        within_limit(&mut tx, &at.profile.0, &origin.principal).await?;
        let id = insert(
            &mut tx,
            &at.profile.0,
            &New {
                origin: &origin,
                fire_origin: bot_core::state::RunOrigin::Wake,
                call: (&at.run.0, &call.0),
                destination: None,
                timed,
                instruction: &wake.instruction,
                quiet: wake.quiet,
                depth,
                now,
            },
        )
        .await?;
        sqlx::query(
            "update bot_calls set state = 'done', result = $4, finished_ms = bot_now_ms(), updated_ms = bot_now_ms()
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&call.0)
        .bind(&result_text)
        .execute(&mut *tx)
        .await?;
        let hash = to32(row.get("args_sha256"))?;
        audit::append_many(
            &mut tx,
            &[
                OwnedEntry::new(&at.profile.0, &origin.principal, "schedule.wake", &id, "at"),
                done_entry(&at.profile.0, &origin.principal, &call.0, hash),
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(ScheduleId(id))
    }
}
