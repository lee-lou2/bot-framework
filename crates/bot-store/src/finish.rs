//! Run completion, parking, waking, and reply outbox enqueue; lock order: parent run -> child run -> audit head.

use crate::{
    audit::{self, OwnedEntry},
    codec::*,
    db::{R, Store, Tx},
    replies,
};
use bot_core::{
    command::{Completion, Finish, RunOutcome},
    id::{Attempt, ClaimEpoch},
    port::Error,
    schedule::quiet_completion,
};
use serde_json::{Value, json};
use sqlx::Row;

pub(crate) struct RunRow {
    pub profile: String,
    pub id: String,
    pub parent: Option<String>,
    pub parent_step: Option<String>,
    pub attempts: i32,
    pub cancel_requested: bool,
    pub private: bool,
}

pub(crate) enum End<'a> {
    Succeeded(&'a str),
    Dead(&'static str, i32),
    Cancelled(&'static str),
}

pub(crate) enum Wait {
    Child(String),
    Approval,
    Hold,
}

/// Locks the parent run first to enforce parent -> child lock order.
pub(crate) async fn lock_parent(tx: &mut Tx<'_>, profile: &str, parent: Option<&str>) -> R<()> {
    if let Some(parent) = parent {
        sqlx::query("select 1 from bot_runs where profile = $1 and id = $2 for update")
            .bind(profile)
            .bind(parent)
            .fetch_optional(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Non-blocking parent lock (`NOWAIT`); returns `false` on lock contention (`55P03`) leaving the tx aborted for caller rollback.
pub(crate) async fn try_lock_parent(
    tx: &mut Tx<'_>,
    profile: &str,
    parent: Option<&str>,
) -> R<bool> {
    let Some(parent) = parent else {
        return Ok(true);
    };
    let locked =
        sqlx::query("select 1 from bot_runs where profile = $1 and id = $2 for update nowait")
            .bind(profile)
            .bind(parent)
            .fetch_optional(&mut **tx)
            .await;
    match locked {
        Ok(_) => Ok(true),
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("55P03") => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub(crate) async fn parent_of(tx: &mut Tx<'_>, at: &Attempt) -> R<Option<String>> {
    let row = sqlx::query("select parent from bot_runs where profile = $1 and id = $2")
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.and_then(|r| r.get::<Option<String>, _>("parent")))
}

fn run_row(row: &sqlx::postgres::PgRow) -> RunRow {
    RunRow {
        profile: row.get("profile"),
        id: row.get("id"),
        parent: row.get("parent"),
        parent_step: row.get("parent_step"),
        attempts: row.get("attempts"),
        cancel_requested: row.get("cancel_requested"),
        private: row.get("private"),
    }
}

/// Locks the run and checks active lease, generation, and claim epoch using DB time read after acquiring the lock.
pub(crate) async fn lock_leased(tx: &mut Tx<'_>, at: &Attempt) -> R<RunRow> {
    let row = sqlx::query(
        "select profile, id, parent, parent_step, attempts, cancel_requested, private, state, generation, claim_epoch,
                lease_until_ms
         from bot_runs where profile = $1 and id = $2 for update",
    )
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Err(Error::Stale.into());
    };
    let now: i64 = sqlx::query_scalar("select bot_now_ms()")
        .fetch_one(&mut **tx)
        .await?;
    let lease: Option<i64> = row.get("lease_until_ms");
    let live = row.get::<String, _>("state") == "leased"
        && row.get::<i64, _>("generation") as u64 == at.generation.0
        && at.claim == Some(ClaimEpoch(row.get::<i64, _>("claim_epoch") as u64))
        && lease.is_some_and(|until| until >= now);
    if !live {
        return Err(Error::Stale.into());
    }
    Ok(run_row(&row))
}

/// Wakes a waiting run (`waiting -> ready`) while preserving its generation.
pub(crate) async fn wake(
    tx: &mut Tx<'_>,
    profile: &str,
    run: &str,
    kind: &str,
    child: Option<&str>,
) -> R<bool> {
    let woke = sqlx::query(
        "update bot_runs set state = 'ready', wait_kind = null, wait_child = null, keep_generation = true,
                updated_ms = bot_now_ms()
         where profile = $1 and id = $2 and state = 'waiting' and wait_kind = $3 and wait_child is not distinct from $4",
    )
    .bind(profile)
    .bind(run)
    .bind(kind)
    .bind(child)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(woke == 1)
}

/// Parks a locked leased run into `waiting` and releases its lease without spending an attempt.
pub(crate) async fn park(tx: &mut Tx<'_>, run: &RunRow, wait: &Wait) -> R<()> {
    let (kind, child) = match wait {
        Wait::Child(child) => ("child", Some(child.as_str())),
        Wait::Approval => ("approval", None),
        Wait::Hold => ("hold", None),
    };
    sqlx::query(
        "update bot_runs set state = 'waiting', wait_kind = $3, wait_child = $4, lease_until_ms = null, lease_owner = null,
                updated_ms = bot_now_ms()
         where profile = $1 and id = $2",
    )
    .bind(&run.profile)
    .bind(&run.id)
    .bind(kind)
    .bind(child)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Ends a locked run (caller already holds parent and child locks), checkpoints the result into a waiting parent once, and queues audits.
pub(crate) async fn end_run_into(
    tx: &mut Tx<'_>,
    run: &RunRow,
    end: End<'_>,
    actor: &str,
    audits: &mut Vec<OwnedEntry>,
) -> R<()> {
    // Reject success if cancelled so companion writes in the same transaction roll back together.
    if run.cancel_requested && matches!(end, End::Succeeded(_)) {
        return Err(Error::Stale.into());
    }
    let (state, result, error, action, attempts) = match &end {
        End::Succeeded(result) => (
            "succeeded",
            Some(*result),
            None,
            "run.succeeded",
            run.attempts,
        ),
        End::Dead(code, attempts) => ("dead", None, Some(*code), "run.dead", *attempts),
        End::Cancelled(code) => (
            "cancelled",
            None,
            Some(*code),
            "run.cancelled",
            run.attempts,
        ),
    };
    sqlx::query(
        "update bot_runs set state = $3, result = $4, error = $5, attempts = $6, lease_until_ms = null, lease_owner = null,
                wait_kind = null, wait_child = null, finished_ms = bot_now_ms(), updated_ms = bot_now_ms()
         where profile = $1 and id = $2",
    )
    .bind(&run.profile)
    .bind(&run.id)
    .bind(state)
    .bind(result)
    .bind(error)
    .bind(attempts)
    .execute(&mut **tx)
    .await?;
    crate::approvals::close_for_run(tx, &run.profile, &run.id).await?;
    // Close open tool holds only; outbox reply delivery holds remain independent.
    sqlx::query(
        "update bot_holds set closed_ms = bot_now_ms()
         where profile = $1 and run = $2 and kind = 'tool' and closed_ms is null",
    )
    .bind(&run.profile)
    .bind(&run.id)
    .execute(&mut **tx)
    .await?;
    let mut woke = None;
    if let (Some(parent), Some(step)) = (&run.parent, &run.parent_step)
        && wake(tx, &run.profile, parent, "child", Some(&run.id)).await?
    {
        let outcome = match &end {
            End::Succeeded(result) => {
                json!({"state": "succeeded", "result": serde_json::from_str::<Value>(result).unwrap_or(Value::Null)})
            }
            End::Dead(code, _) => json!({"state": "dead", "error": code}),
            End::Cancelled(code) => json!({"state": "cancelled", "error": code}),
        };
        sqlx::query("insert into bot_steps (profile, run, key, body) values ($1, $2, $3, $4) on conflict do nothing")
            .bind(&run.profile)
            .bind(parent)
            .bind(step)
            .bind(outcome.to_string())
            .execute(&mut **tx)
            .await?;
        woke = Some(parent.clone());
    }
    audits.push(OwnedEntry::new(
        &run.profile,
        actor,
        action,
        &run.id,
        error.unwrap_or("ok"),
    ));
    if let Some(parent) = woke {
        audits.push(OwnedEntry::new(
            &run.profile,
            actor,
            "run.wake",
            parent,
            &run.id,
        ));
    }
    Ok(())
}

pub(crate) async fn end_run(tx: &mut Tx<'_>, run: &RunRow, end: End<'_>, actor: &str) -> R<()> {
    let mut audits = Vec::new();
    end_run_into(tx, run, end, actor, &mut audits).await?;
    audit::append_many(tx, &audits).await
}

/// Opens a reply hold immediately after inserting a missing-transport outbox row (`bot_outbox_hold` triggers on `UPDATE` only).
pub(crate) async fn hold_missing_transport_into(
    tx: &mut Tx<'_>,
    profile: &str,
    run: &str,
    call: &str,
    audits: &mut Vec<OwnedEntry>,
) -> R<()> {
    sqlx::query("select bot_hold_open($1, $2, $3, null, 'reply')")
        .bind(profile)
        .bind(run)
        .bind(call)
        .execute(&mut **tx)
        .await?;
    audits.push(OwnedEntry::new(
        profile,
        "system",
        "outbox.held",
        call,
        "missing_transport",
    ));
    Ok(())
}

/// Enqueues a reply row for push conversations (`direct`/`channel`) only, opening a hold if `reply_transport` is missing.
pub(crate) async fn push_reply_into(
    tx: &mut Tx<'_>,
    profile: &str,
    run: &str,
    body: &str,
    audits: &mut Vec<OwnedEntry>,
) -> R<()> {
    let inserted: Option<(String, String)> = sqlx::query_as(
        "insert into bot_outbox (profile, run, seq, call, binding, principal, conv_kind, conv_id, thread, body, state, reason, transport)
         select r.profile, r.id, n.seq, 'reply:' || r.id || ':' || n.seq::text, r.binding, r.principal,
                r.conv_kind, r.conv_id, r.thread, $3,
                case when coalesce(r.reply_transport, '') = '' then 'held' else 'pending' end,
                case when coalesce(r.reply_transport, '') = '' then 'missing_transport' else null end,
                coalesce(r.reply_transport, '')
         from bot_runs r,
              lateral (select coalesce(max(o.seq) + 1, 0) as seq from bot_outbox o where o.profile = r.profile and o.run = r.id) n
         where r.profile = $1 and r.id = $2 and not r.private and r.conv_kind in ('direct', 'channel')
         returning call, state",
    )
    .bind(profile)
    .bind(run)
    .bind(body)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((call, state)) = inserted
        && state == "held"
    {
        hold_missing_transport_into(tx, profile, run, &call, audits).await?;
    }
    Ok(())
}

pub(crate) async fn push_reply(tx: &mut Tx<'_>, profile: &str, run: &str, body: &str) -> R<()> {
    let mut audits = Vec::new();
    push_reply_into(tx, profile, run, body, &mut audits).await?;
    audit::append_many(tx, &audits).await
}

impl Store {
    pub(crate) async fn finish_run(&self, at: &Attempt, done: Finish) -> R<Completion> {
        self.finish_run_with(at, done, None).await
    }

    /// Completes a run atomically with outbox replies, parent wake, and optional usage/truncation telemetry.
    pub(crate) async fn finish_run_with(
        &self,
        at: &Attempt,
        done: Finish,
        outcome: Option<RunOutcome>,
    ) -> R<Completion> {
        let mut tx = self.pool.begin().await?;
        let parent = parent_of(&mut tx, at).await?;
        lock_parent(&mut tx, &at.profile.0, parent.as_deref()).await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private {
            return Err(Error::Denied.into());
        }
        if run.cancel_requested {
            end_run(&mut tx, &run, End::Cancelled("cancelled"), "system").await?;
            tx.commit().await?;
            return Ok(Completion::Cancelled);
        }
        let (_, result) = canonical(&done.result)?;
        let answers = done
            .answers
            .iter()
            .map(replies::answer)
            .collect::<R<Vec<_>>>()?;
        if answers.len() > 16 {
            return invalid();
        }
        // Explicit-destination scheduled runs must settle via `ScheduledReplies`, unless a quiet attachment-free run dropped its draft.
        let delivery: Option<String> =
            sqlx::query_scalar("select state from bot_deliveries where profile = $1 and run = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_optional(&mut *tx)
                .await?;
        if delivery.is_some_and(|state| state != "dropped") {
            return Err(Error::Conflict.into());
        }
        let mut audits = Vec::new();
        end_run_into(
            &mut tx,
            &run,
            End::Succeeded(&result),
            "system",
            &mut audits,
        )
        .await?;
        if let Some(outcome) = outcome {
            sqlx::query(
                "update bot_runs set truncated = $3, usage_prompt = $4, usage_completion = $5 where profile = $1 and id = $2",
            )
            .bind(&run.profile)
            .bind(&run.id)
            .bind(outcome.truncated)
            .bind(outcome.usage.map(|u| i64::try_from(u.prompt).unwrap_or(i64::MAX)))
            .bind(outcome.usage.map(|u| i64::try_from(u.completion).unwrap_or(i64::MAX)))
            .execute(&mut *tx)
            .await?;
        }
        // Suppress replies only for quiet schedules whose final text is `<quiet>` with no attachments.
        let quiet: bool =
            sqlx::query_scalar("select quiet from bot_runs where profile = $1 and id = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_one(&mut *tx)
                .await?;
        let has_attachments = answers.iter().any(|a| has_attachments(a));
        let silent = quiet_completion(quiet, &final_text(&result)) && !has_attachments;
        if run.parent.is_none() && !silent {
            for body in &answers {
                push_reply_into(&mut tx, &run.profile, &run.id, body, &mut audits).await?;
            }
        }
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(Completion::Succeeded)
    }
}

pub(crate) fn final_text(result: &str) -> String {
    serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|v| {
            v.get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

pub(crate) fn has_attachments(answer: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(answer)
        .ok()
        .and_then(|v| {
            v.get("attachments")
                .and_then(serde_json::Value::as_array)
                .map(|a| !a.is_empty())
        })
        .unwrap_or(false)
}
