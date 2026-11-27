//! User schedule creation and cancellation (`UserSchedules`), bound to stored run identity and approval-fenced commits.
//!
//! Active schedules are capped at `MAX_ACTIVE_PER_OWNER` per `(profile, owner)` via an owner advisory lock,
//! and chained scheduled runs stop at `MAX_WAKE_DEPTH`. Firing lives in `user_due.rs`; list/wake live in `user_wake.rs`.

use crate::{
    audit::{self, OwnedEntry},
    calls::{fence, lock_call, lock_root_shared, phase_of, risk_str, tree_flagged},
    codec::*,
    db::{R, Store, Tx, ms_to_time, time_to_ms},
    finish::RunRow,
};
use bot_core::{
    id::*,
    ledger::{CallPhase, StartCall},
    port::Error,
    schedule::*,
    state::Conversation,
};
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgRow};

pub(crate) use bot_core::builtin::{CANCEL_TOOL, CREATE_TOOL};

use bot_core::builtin::MAX_SCHEDULE_AHEAD_MS as MAX_AHEAD_MS;

pub(crate) fn result_of(id: &str) -> String {
    json!({ "schedule": id }).to_string()
}

/// Canonical JCS SHA-256 of `{"id": ...}` matching broker approval sealing.
pub(crate) fn cancel_hash(id: &ScheduleId) -> [u8; 32] {
    let args = json!({ "id": id.0 });
    bot_core::canon::sha256(&bot_core::canon::canonicalize(&args).unwrap_or_default())
}

/// Built-in tools that must execute through dedicated atomic store ops rather than generic `CallLedger::start`.
pub(crate) fn dedicated_tool(tool: &str) -> bool {
    matches!(
        tool,
        crate::share::SHARE_TOOL | CREATE_TOOL | CANCEL_TOOL | DELIVER_TOOL
    )
}

pub(crate) fn id_in(result: &str) -> Option<String> {
    serde_json::from_str::<Value>(result)
        .ok()?
        .get("schedule")?
        .as_str()
        .map(str::to_owned)
}

pub(crate) struct Timed {
    at_ms: Option<i64>,
    cron: Option<String>,
    timezone: Option<String>,
    next_ms: i64,
}

pub(crate) fn timed(timing: &Timing, now: i64) -> R<Timed> {
    match timing {
        Timing::At(at) => {
            let ms = time_to_ms(*at);
            if ms <= now || ms > now + MAX_AHEAD_MS {
                return invalid();
            }
            Ok(Timed {
                at_ms: Some(ms),
                cron: None,
                timezone: None,
                next_ms: ms,
            })
        }
        Timing::Cron {
            expression,
            timezone,
        } => {
            if !id_ok(timezone)
                || !valid_iana_timezone(timezone)
                || timezone.parse::<chrono_tz::Tz>().is_err()
            {
                return invalid();
            }
            Ok(Timed {
                at_ms: None,
                cron: Some(expression.to_string()),
                timezone: Some(timezone.clone()),
                // Scheduler computes the first cron occurrence, so make it immediately eligible for scan.
                next_ms: now,
            })
        }
    }
}

pub(crate) fn instruction_ok(text: &str) -> R<()> {
    if text.trim().is_empty() || text.len() > MAX_INSTRUCTION_BYTES || text.contains('\0') {
        return invalid();
    }
    Ok(())
}

pub(crate) struct Origin {
    pub(crate) principal: String,
    pub(crate) agent: String,
    pub(crate) binding: String,
    pub(crate) kind: String,
    pub(crate) conv_id: String,
    pub(crate) thread: Option<String>,
    pub(crate) scopes: Vec<String>,
    pub(crate) approver: Option<String>,
    pub(crate) tainted: bool,
    /// Chained automatic wake depth inherited from the root run.
    pub(crate) depth: i16,
    /// True when the root run was fired by a schedule rather than a direct user request.
    pub(crate) scheduled: bool,
}

pub(crate) async fn origin_of(
    tx: &mut Tx<'_>,
    at: &Attempt,
    run: &RunRow,
    shared_flag: Option<bool>,
) -> R<Origin> {
    let me = sqlx::query(
        "select principal, agent, binding, conv_kind, conv_id, thread, scopes, approver, root from bot_runs
         where profile = $1 and id = $2",
    )
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .fetch_one(&mut **tx)
    .await?;
    let kind: String = me.get("conv_kind");
    // HTTP callers have no push channel for deferred replies.
    if kind == "http" {
        return Err(Error::Denied.into());
    }
    let root: String = me.get("root");
    let (depth, scheduled): (i16, bool) = sqlx::query_as(
        "select wake_depth, trigger is not null from bot_runs where profile = $1 and id = $2",
    )
    .bind(&at.profile.0)
    .bind(&root)
    .fetch_one(&mut **tx)
    .await?;
    Ok(Origin {
        principal: me.get("principal"),
        agent: me.get("agent"),
        binding: me.get("binding"),
        kind,
        conv_id: me.get("conv_id"),
        thread: me.get("thread"),
        scopes: me.get("scopes"),
        approver: me.get("approver"),
        tainted: tree_flagged(tx, run, shared_flag).await?,
        depth,
        scheduled,
    })
}

pub(crate) enum Claimed {
    /// Replay of an already-finished call with its recorded result.
    Done(String),
    Fresh {
        run: RunRow,
        shared_flag: Option<bool>,
        approval: Option<String>,
    },
}

/// Runs `CallLedger::start` checks (active revision -> root run -> run -> call -> private flag -> approval consumption)
/// and verifies the sealed args digest before consuming approval or returning a replayed result.
pub(crate) async fn claim(
    tx: &mut Tx<'_>,
    s: &StartCall,
    tool: &str,
    sealed: &[u8; 32],
) -> R<Claimed> {
    let at = &s.attempt;
    crate::active::guard_active(tx, &at.profile.0, &s.checked_revision).await?;
    let shared_flag = lock_root_shared(tx, at).await?;
    let run = fence(tx, at).await?;
    let call = lock_call(tx, at, &s.call).await?;
    // Ensure the ledger call matches this exact built-in tool.
    if call.get::<String, _>("tool") != tool {
        return Err(Error::Denied.into());
    }
    if call.get::<Vec<u8>, _>("args_sha256").as_slice() != sealed.as_slice() {
        return Err(Error::Conflict.into());
    }
    let pointer = match phase_of(&call)? {
        CallPhase::Done(result) => return Ok(Claimed::Done(result.0)),
        CallPhase::Open { approval } => approval,
        _ => return Err(Error::Conflict.into()),
    };
    if tree_flagged(tx, &run, shared_flag).await? && !s.private_recall {
        return Err(Error::Conflict.into()); // Another run in the tree read private memory after policy evaluation.
    }
    crate::approvals::require_presented(tx, pointer.as_ref(), s.approval.is_some()).await?;
    let approval = match &s.approval {
        Some((expected, reasons)) => {
            crate::approvals::consume(tx, at, &s.call, expected, reasons).await?;
            Some(expected.nonce.0.clone())
        }
        None => None,
    };
    Ok(Claimed::Fresh {
        run,
        shared_flag,
        approval,
    })
}

pub(crate) async fn finish_call(
    tx: &mut Tx<'_>,
    s: &StartCall,
    result: &str,
    approval: Option<&str>,
) -> R<()> {
    let at = &s.attempt;
    sqlx::query(
        "update bot_calls set state = 'done', result = $4, started_generation = $5, owner = $6, started_ms = bot_now_ms(),
                finished_ms = bot_now_ms(), updated_ms = bot_now_ms(), risk = $7, tool_revision = $8,
                approval = coalesce($9, approval)
         where profile = $1 and run = $2 and call = $3",
    )
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .bind(&s.call.0)
    .bind(result)
    .bind(at.generation.0 as i64)
    .bind(&s.owner)
    .bind(risk_str(s.risk))
    .bind(&s.tool_revision.0)
    .bind(approval)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) fn done_entry(profile: &str, actor: &str, call: &str, hash: [u8; 32]) -> OwnedEntry {
    let mut done = OwnedEntry::new(profile, actor, "call.done", call, "done");
    done.args = Some(hash);
    done
}

/// Serializes per-owner quota checks via advisory lock and enforces `MAX_ACTIVE_PER_OWNER`.
pub(crate) async fn within_limit(tx: &mut Tx<'_>, profile: &str, owner: &str) -> R<()> {
    sqlx::query(
        "select pg_advisory_xact_lock(hashtextextended('bot.userschedule:' || $1 || ':' || $2, 0))",
    )
    .bind(profile)
    .bind(owner)
    .execute(&mut **tx)
    .await?;
    let active: i64 = sqlx::query_scalar(
        "select count(*) from bot_user_schedules where profile = $1 and owner = $2 and state = 'active'",
    )
    .bind(profile)
    .bind(owner)
    .fetch_one(&mut **tx)
    .await?;
    if active as usize >= MAX_ACTIVE_PER_OWNER {
        return Err(Error::Denied.into());
    }
    Ok(())
}

pub(crate) struct New<'a> {
    pub(crate) origin: &'a Origin,
    pub(crate) fire_origin: bot_core::state::RunOrigin,
    pub(crate) call: (&'a str, &'a str),
    pub(crate) destination: Option<&'a DeliveryTarget>,
    pub(crate) timed: Timed,
    pub(crate) instruction: &'a str,
    pub(crate) quiet: bool,
    pub(crate) depth: i16,
    pub(crate) now: i64,
}

pub(crate) async fn insert(tx: &mut Tx<'_>, profile: &str, n: &New<'_>) -> R<String> {
    let id: String = sqlx::query_scalar("select gen_random_uuid()::text")
        .fetch_one(&mut **tx)
        .await?;
    let dest = n.destination;
    sqlx::query(
        "insert into bot_user_schedules
            (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id, origin_thread,
             origin_private_read, approver, ceiling, dest_binding, dest_kind, dest_id, dest_thread,
             at_ms, cron, timezone, instruction, quiet, wake_depth, cursor_ms, next_ms, fire_origin)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26)",
    )
    .bind(profile)
    .bind(&id)
    .bind(&n.origin.principal)
    .bind(&n.origin.agent)
    .bind(n.call.0)
    .bind(n.call.1)
    .bind(&n.origin.binding)
    .bind(&n.origin.kind)
    .bind(&n.origin.conv_id)
    .bind(&n.origin.thread)
    .bind(n.origin.tainted)
    .bind(&n.origin.approver)
    .bind(&n.origin.scopes)
    .bind(dest.map(|d| d.binding.0.as_str()))
    .bind(dest.map(|d| kind_str(&d.conversation.kind)))
    .bind(dest.map(|d| d.conversation.id.as_str()))
    .bind(dest.and_then(|d| d.conversation.thread.as_deref()))
    .bind(n.timed.at_ms)
    .bind(&n.timed.cron)
    .bind(&n.timed.timezone)
    .bind(n.instruction)
    .bind(n.quiet)
    .bind(n.depth)
    .bind(n.now)
    .bind(n.timed.next_ms)
    .bind(n.fire_origin.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

/// Explicit destinations allow only push conversations (`direct`/`channel`).
/// Host routing (`bot-gateway/src/schedule_routes.rs`) and `delivery::push_to` currently support Slack only;
/// adding another push adapter will require storing a `dest_transport` key.
pub(crate) fn destination_ok(dest: &DeliveryTarget) -> R<()> {
    ids_ok(&[&dest.binding.0, &dest.conversation.id])?;
    if dest
        .conversation
        .thread
        .as_deref()
        .is_some_and(|t| !id_ok(t))
    {
        return invalid();
    }
    match dest.conversation.kind {
        bot_core::state::ConversationKind::Direct | bot_core::state::ConversationKind::Channel => {
            Ok(())
        }
        bot_core::state::ConversationKind::Cli
        | bot_core::state::ConversationKind::Http
        | bot_core::state::ConversationKind::Scheduled => invalid(),
    }
}

pub(crate) fn summary_of(row: &PgRow) -> R<ScheduleSummary> {
    let timing = match (
        row.get::<Option<i64>, _>("at_ms"),
        row.get::<Option<String>, _>("cron"),
    ) {
        (Some(at), None) => Timing::At(ms_to_time(at)),
        (None, Some(cron)) => Timing::Cron {
            expression: bot_core::cron::Cron::parse(&cron).map_err(|_| Error::Invalid)?,
            timezone: row.get("timezone"),
        },
        _ => return invalid(),
    };
    Ok(ScheduleSummary {
        id: ScheduleId(row.get("id")),
        timing,
        quiet: row.get("quiet"),
    })
}

impl Store {
    pub(crate) async fn schedule_create(&self, s: StartCall, spec: ScheduleSpec) -> R<ScheduleId> {
        ids_ok(&[&s.owner, &s.checked_revision.0, &s.call.0])?;
        instruction_ok(&spec.instruction)?;
        // Reject unsealable specs (invalid timezone or timestamp) before opening a transaction.
        let sealed = spec.seal_create()?.sha256;
        // Explicit-destination replies require owner approval on every occurrence via `ScheduledReplies`.
        if let Some(dest) = &spec.destination {
            destination_ok(dest)?;
        }
        let at = &s.attempt;
        let mut tx = self.begin_authorized().await?;
        let (run, shared_flag, approval) = match claim(&mut tx, &s, CREATE_TOOL, &sealed).await? {
            Claimed::Done(result) => {
                tx.rollback().await?;
                return id_in(&result).map(ScheduleId).ok_or(Error::Conflict.into());
            }
            Claimed::Fresh {
                run,
                shared_flag,
                approval,
            } => (run, shared_flag, approval),
        };
        // Explicit-destination schedules require an approved creation call.
        if spec.destination.is_some() && approval.is_none() {
            return Err(Error::Denied.into());
        }
        let origin = origin_of(&mut tx, at, &run, shared_flag).await?;
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let timed = timed(&spec.timing, now)?;
        // Only direct user requests start at depth 0; scheduled runs (and their descendants) increment root depth.
        let depth = if origin.scheduled {
            origin.depth + 1
        } else {
            0
        };
        if depth > i16::from(MAX_WAKE_DEPTH) {
            return Err(Error::Denied.into());
        }
        within_limit(&mut tx, &at.profile.0, &origin.principal).await?;
        let id = insert(
            &mut tx,
            &at.profile.0,
            &New {
                origin: &origin,
                fire_origin: bot_core::state::RunOrigin::Schedule,
                call: (&at.run.0, &s.call.0),
                destination: spec.destination.as_ref(),
                timed,
                instruction: &spec.instruction,
                quiet: spec.quiet,
                depth,
                now,
            },
        )
        .await?;
        finish_call(&mut tx, &s, &result_of(&id), approval.as_deref()).await?;
        let kind = if matches!(spec.timing, Timing::At(_)) {
            "at"
        } else {
            "cron"
        };
        audit::append_many(
            &mut tx,
            &[
                OwnedEntry::new(
                    &at.profile.0,
                    &origin.principal,
                    "schedule.create",
                    &id,
                    kind,
                ),
                done_entry(&at.profile.0, &origin.principal, &s.call.0, sealed),
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(ScheduleId(id))
    }

    pub(crate) async fn schedule_cancel(&self, s: StartCall, id: ScheduleId) -> R<()> {
        ids_ok(&[&s.owner, &s.checked_revision.0, &s.call.0, &id.0])?;
        let at = &s.attempt;
        let sealed = cancel_hash(&id);
        let mut tx = self.begin_authorized().await?;
        let (run, shared_flag, approval) = match claim(&mut tx, &s, CANCEL_TOOL, &sealed).await? {
            Claimed::Done(result) => {
                tx.rollback().await?;
                return if id_in(&result).as_deref() == Some(id.0.as_str()) {
                    Ok(())
                } else {
                    Err(Error::Conflict.into())
                };
            }
            Claimed::Fresh {
                run,
                shared_flag,
                approval,
            } => (run, shared_flag, approval),
        };
        let origin = origin_of(&mut tx, at, &run, shared_flag).await?;
        // Return `Missing` for both nonexistent and non-owned schedules to avoid leaking existence.
        let row = sqlx::query(
            "select owner, state from bot_user_schedules where profile = $1 and id = $2 for update",
        )
        .bind(&at.profile.0)
        .bind(&id.0)
        .fetch_optional(&mut *tx)
        .await?
        .filter(|r| r.get::<String, _>("owner") == origin.principal)
        .ok_or(Error::Missing)?;
        let state: String = row.get("state");
        if state == "active" {
            sqlx::query(
                "update bot_user_schedules set state = 'canceled', close_reason = 'canceled', next_ms = null, updated_ms = bot_now_ms()
                 where profile = $1 and id = $2",
            )
            .bind(&at.profile.0)
            .bind(&id.0)
            .execute(&mut *tx)
            .await?;
        }
        finish_call(&mut tx, &s, &result_of(&id.0), approval.as_deref()).await?;
        let verdict = if state == "active" {
            "canceled"
        } else {
            "already_closed"
        };
        audit::append_many(
            &mut tx,
            &[
                OwnedEntry::new(
                    &at.profile.0,
                    &origin.principal,
                    "schedule.cancel",
                    &id.0,
                    verdict,
                ),
                done_entry(&at.profile.0, &origin.principal, &s.call.0, sealed),
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

pub(crate) fn origin_target(row: &PgRow) -> R<DeliveryTarget> {
    Ok(DeliveryTarget {
        binding: BindingId(row.get("origin_binding")),
        conversation: Conversation {
            kind: kind_from(&row.get::<String, _>("origin_kind"))?,
            id: row.get("origin_id"),
            thread: row.get("origin_thread"),
        },
    })
}

pub(crate) fn destination_target(row: &PgRow) -> R<Option<DeliveryTarget>> {
    let Some(binding) = row.get::<Option<String>, _>("dest_binding") else {
        return Ok(None);
    };
    Ok(Some(DeliveryTarget {
        binding: BindingId(binding),
        conversation: Conversation {
            kind: kind_from(
                &row.get::<Option<String>, _>("dest_kind")
                    .ok_or(Error::Invalid)?,
            )?,
            id: row
                .get::<Option<String>, _>("dest_id")
                .ok_or(Error::Invalid)?,
            thread: row.get("dest_thread"),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::timed;
    use bot_core::{cron::Cron, port::Error, schedule::Timing};

    #[test]
    fn timed_rejects_non_canonical_iana_timezone_matching_bot_core() {
        let expr = Cron::parse("0 9 * * *").unwrap();
        for ok in ["UTC", "Asia/Seoul", "America/New_York"] {
            let t = Timing::Cron {
                expression: expr.clone(),
                timezone: ok.into(),
            };
            assert!(timed(&t, 1_700_000_000_000).is_ok(), "{ok}");
        }
        // Non-canonical casing and unknown names rejected by bot-core's `valid_iana_timezone` must fail here too.
        for bad in ["asia/seoul", "utc", "Mars/Phobos"] {
            let t = Timing::Cron {
                expression: expr.clone(),
                timezone: bad.into(),
            };
            assert!(
                matches!(
                    timed(&t, 1_700_000_000_000),
                    Err(crate::db::Fail::Core(Error::Invalid))
                ),
                "{bad}"
            );
        }
    }
}
