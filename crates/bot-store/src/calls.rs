//! Call ledger transitions (`context`, `lookup`, `begin`, `start`, `finish`) fenced by active lease, generation, and cancellation state.

use crate::{
    audit::{self, Entry, OwnedEntry},
    codec::*,
    db::{Fail, R, Store, Tx},
    finish::*,
    queue,
};
use bot_core::{
    command::JsonPayload,
    id::*,
    ledger::*,
    port::Error,
    state::{Risk, RunContext},
};
use sqlx::{Row, postgres::PgRow};

pub(crate) fn risk_str(risk: Risk) -> &'static str {
    match risk {
        Risk::Read => "read",
        Risk::Write => "write",
        Risk::Irreversible => "irreversible",
    }
}

pub(crate) fn risk_from(s: &str) -> R<Risk> {
    Ok(match s {
        "read" => Risk::Read,
        "write" => Risk::Write,
        "irreversible" => Risk::Irreversible,
        _ => return invalid(),
    })
}

/// Verifies active lease, generation, claim epoch, and no pending cancel; rejects private runs with `Denied`.
/// Checks the caller's profile first so same-ID runs in another profile do not mask `Stale` as `Denied`.
pub(crate) async fn fence(tx: &mut Tx<'_>, at: &Attempt) -> R<RunRow> {
    match lock_leased(tx, at).await {
        Ok(run) if run.private => Err(Error::Denied.into()),
        Ok(run) if run.cancel_requested => Err(Error::Stale.into()),
        Ok(run) => Ok(run),
        Err(Fail::Core(Error::Stale)) => {
            // Check caller's profile PK first to avoid a cross-profile scan on normal expiry.
            let mine: bool = sqlx::query_scalar(
                "select exists (select 1 from bot_runs where profile = $1 and id = $2)",
            )
            .bind(&at.profile.0)
            .bind(&at.run.0)
            .fetch_one(&mut **tx)
            .await?;
            if mine {
                return Err(Error::Stale.into());
            }
            let other: bool =
                sqlx::query_scalar("select exists (select 1 from bot_runs where id = $1)")
                    .bind(&at.run.0)
                    .fetch_one(&mut **tx)
                    .await?;
            Err(if other { Error::Denied } else { Error::Missing }.into())
        }
        Err(other) => Err(other),
    }
}

/// Settlement lease fence (`finish`, effect recording): permits in-flight settlement even after `cancel_requested` to prevent lost receipts.
pub(crate) async fn fence_settle(tx: &mut Tx<'_>, at: &Attempt) -> R<RunRow> {
    match fence(tx, at).await {
        Err(Fail::Core(Error::Stale)) => lock_leased_ignoring_cancel(tx, at).await,
        other => other,
    }
}

async fn lock_leased_ignoring_cancel(tx: &mut Tx<'_>, at: &Attempt) -> R<RunRow> {
    let run = lock_leased(tx, at).await?;
    if run.private {
        return Err(Error::Denied.into());
    }
    Ok(run)
}

pub(crate) fn phase_of(row: &PgRow) -> R<CallPhase> {
    let text = |col: &str| row.get::<Option<String>, _>(col);
    Ok(match row.get::<String, _>("state").as_str() {
        "open" => CallPhase::Open {
            approval: text("approval").map(ApprovalId),
        },
        "started" => CallPhase::Started {
            generation: Generation(
                row.get::<Option<i64>, _>("started_generation").unwrap_or(0) as u64
            ),
            owner: text("owner").unwrap_or_default(),
            confirmed_step: row.get("confirmed_step"),
        },
        "done" => CallPhase::Done(JsonPayload(text("result").unwrap_or_default())),
        "denied" => CallPhase::Denied,
        "held" => CallPhase::Held {
            reason: text("reason").unwrap_or_default(),
            resolution: match text("resolution").as_deref() {
                Some("executed") => Some(Resolution::Executed {
                    receipt: text("receipt").unwrap_or_default(),
                }),
                Some("not_executed") => Some(Resolution::NotExecuted),
                _ => None,
            },
        },
        _ => return invalid(),
    })
}

pub(crate) fn record_of(row: &PgRow) -> R<CallRecord> {
    Ok(CallRecord {
        call: CallId(row.get("call")),
        tool: ToolId(row.get("tool")),
        args_sha256: to32(row.get("args_sha256"))?,
        phase: phase_of(row)?,
    })
}

/// For delegated children, locks the root run `FOR SHARE` to read `private_memory_read`; roots return `None` to avoid shared-to-exclusive deadlock.
pub(crate) async fn lock_root_shared(tx: &mut Tx<'_>, at: &Attempt) -> R<Option<bool>> {
    let root: Option<String> =
        sqlx::query_scalar("select root from bot_runs where profile = $1 and id = $2")
            .bind(&at.profile.0)
            .bind(&at.run.0)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(root) = root.filter(|r| *r != at.run.0) else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar(
        "select private_memory_read from bot_runs where profile = $1 and id = $2 for share",
    )
    .bind(&at.profile.0)
    .bind(&root)
    .fetch_optional(&mut **tx)
    .await?)
}

pub(crate) async fn tree_flagged(
    tx: &mut Tx<'_>,
    run: &RunRow,
    shared_flag: Option<bool>,
) -> R<bool> {
    Ok(match shared_flag {
        Some(flag) => flag,
        None => {
            sqlx::query_scalar(
                "select private_memory_read from bot_runs where profile = $1 and id = $2",
            )
            .bind(&run.profile)
            .bind(&run.id)
            .fetch_one(&mut **tx)
            .await?
        }
    })
}

// Matches `lock_call` evidence; the active claim holds the run lock so step settlement cannot race.
const CONFIRMED_STEP: &str = "case when c.state in ('started', 'held') then exists (
    select 1 from bot_effects e where e.profile = c.profile and e.run = c.run and e.call = c.call and e.state = 'confirmed'
) else false end as confirmed_step";

pub(crate) async fn lock_call(tx: &mut Tx<'_>, at: &Attempt, call: &CallId) -> R<PgRow> {
    sqlx::query(&format!(
        "select {}, {CONFIRMED_STEP} from bot_calls c where profile = $1 and run = $2 and call = $3 for update of c",
        crate::cols::CALLS
    ))
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .bind(&call.0)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Missing.into())
}

impl Store {
    pub(crate) async fn call_context(&self, at: &Attempt) -> R<RunContext> {
        let mut tx = self.pool.begin().await?;
        self.check_pinned(&mut tx).await?;
        fence(&mut tx, at).await?;
        let context = queue::load(&mut tx, &at.profile.0, &at.run.0).await?;
        tx.rollback().await?;
        Ok(context)
    }

    pub(crate) async fn call_lookup(&self, at: &Attempt, call: &CallId) -> R<Option<CallRecord>> {
        ids_ok(&[&call.0])?;
        let mut tx = self.pool.begin().await?;
        self.check_pinned(&mut tx).await?;
        fence(&mut tx, at).await?;
        let row = sqlx::query(&format!(
            "select {}, {CONFIRMED_STEP} from bot_calls c where profile = $1 and run = $2 and call = $3",
            crate::cols::CALLS
        ))
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&call.0)
        .fetch_optional(&mut *tx)
        .await?;
        tx.rollback().await?;
        row.as_ref().map(record_of).transpose()
    }

    pub(crate) async fn call_step(&self, at: &Attempt, key: &str) -> R<Option<JsonPayload>> {
        ids_ok(&[key])?;
        let row = sqlx::query(
            "with owner as (
                 select private, coalesce(state = 'leased' and not cancel_requested
                     and generation = $3 and claim_epoch = $4 and lease_until_ms >= bot_now_ms()
                     and ($6::text is null or exists (
                         select 1 from bot_active where singleton and revision = $6 and revision <> ''
                     )), false) as live
                 from bot_runs where profile = $1 and id = $2
             )
             select owner.private, owner.live, s.body from owner left join bot_steps s
             on owner.live and not owner.private and s.profile = $1 and s.run = $2 and s.key = $5",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(at.generation.0 as i64)
        .bind(at.claim.map(|c| c.0 as i64))
        .bind(key)
        .bind(self.pinned_revision().map(|revision| revision.0))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::Stale)?;
        if row.get::<bool, _>("private") {
            return Err(Error::Denied.into());
        }
        if !row.get::<bool, _>("live") {
            return Err(Error::Stale.into());
        }
        Ok(row.get::<Option<String>, _>("body").map(JsonPayload))
    }

    /// Reads lease validity and the profile's active revision in one statement without locking.
    pub(crate) async fn call_active_revision(&self, at: &Attempt) -> R<Option<Revision>> {
        let row = sqlx::query(
            "select r.private, r.cancel_requested,
                    (r.state = 'leased' and r.generation = $3 and r.claim_epoch = $4
                     and r.lease_until_ms >= bot_now_ms()) as live,
                    (select p.revision from bot_active_profiles p where p.profile = $1) as active
             from bot_runs r where r.profile = $1 and r.id = $2",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(at.generation.0 as i64)
        .bind(at.claim.map_or(-1, |c| c.0 as i64))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::Stale)?;
        if row.get::<bool, _>("private") {
            return Err(Error::Denied.into());
        }
        if !row.get::<bool, _>("live") || row.get::<bool, _>("cancel_requested") {
            return Err(Error::Stale.into());
        }
        Ok(row.get::<Option<String>, _>("active").map(Revision))
    }

    /// Idempotently opens `(run, call)`; parks the run if an unresolved hold already exists.
    pub(crate) async fn call_begin(
        &self,
        at: &Attempt,
        call: &CallId,
        tool: &ToolId,
        args: [u8; 32],
    ) -> R<CallRecord> {
        ids_ok(&[&call.0, &tool.0])?;
        // Reserve `reply:` and `private:` prefixes for synthetic outbox/private hold IDs.
        if call.0.starts_with("reply:") || call.0.starts_with("private:") {
            return invalid();
        }
        // `DELIVER_CALL` is reserved exclusively for `DELIVER_TOOL`.
        let delivering = tool.0 == bot_core::schedule::DELIVER_TOOL;
        if delivering != (call.0 == bot_core::schedule::DELIVER_CALL) {
            return invalid();
        }
        let mut tx = self.pool.begin().await?;
        let run = fence(&mut tx, at).await?;
        // Require an existing frozen draft matching `args` before opening `DELIVER_TOOL`.
        if delivering {
            crate::delivery::check_begin(&mut tx, &at.profile.0, &at.run.0, &args).await?;
        }
        let fresh = sqlx::query(
            "insert into bot_calls (profile, run, call, tool, args_sha256) values ($1, $2, $3, $4, $5)
             on conflict do nothing",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&call.0)
        .bind(&tool.0)
        .bind(args.as_slice())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        let row = lock_call(&mut tx, at, call).await?;
        let record = record_of(&row)?;
        if !fresh && (record.tool != *tool || record.args_sha256 != args) {
            tx.rollback().await?;
            return Err(Error::Conflict.into());
        }
        if matches!(
            record.phase,
            CallPhase::Held {
                resolution: None,
                ..
            }
        ) {
            park(&mut tx, &run, &Wait::Hold).await?;
            audit::append(
                &mut tx,
                &run.profile,
                &Entry::new("system", "run.wait", &run.id, &format!("hold:{}", call.0)),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(record)
    }

    /// Starts an open call under lease, revision, privacy, and approval checks; refuses grant-bound calls (use `call_start_with`).
    pub(crate) async fn call_start(&self, s: StartCall) -> R<()> {
        self.start_inner(s, None).await.map(|_| ())
    }

    /// Like `call_start`, also verifying pre-approval grant and human-evidence authority under the same lock.
    pub(crate) async fn call_start_with(
        &self,
        s: StartCall,
        authority: bot_core::grant::Authority,
    ) -> R<bot_core::grant::Granted> {
        self.start_inner(s, Some(&authority)).await
    }

    async fn start_inner(
        &self,
        s: StartCall,
        authority: Option<&bot_core::grant::Authority>,
    ) -> R<bot_core::grant::Granted> {
        let at = &s.attempt;
        ids_ok(&[&s.owner, &s.checked_revision.0])?;
        let mut tx = self.begin_authorized().await?;
        // Lock order: active revision -> root run (FOR SHARE) -> run -> call -> approval -> audit.
        crate::active::guard_active(&mut tx, &at.profile.0, &s.checked_revision).await?;
        let shared_flag = lock_root_shared(&mut tx, at).await?;
        let run = fence(&mut tx, at).await?;
        let call = lock_call(&mut tx, at, &s.call).await?;
        // Dedicated tools commit approval consumption, state mutation, and call completion atomically.
        if crate::user_schedule::dedicated_tool(&call.get::<String, _>("tool")) {
            return Err(Error::Denied.into());
        }
        let CallPhase::Open { approval: pointer } = phase_of(&call)? else {
            return Err(Error::Conflict.into());
        };
        let flagged = tree_flagged(&mut tx, &run, shared_flag).await?;
        if flagged && !s.private_recall {
            // Another run read private memory after policy evaluation.
            return Err(Error::Conflict.into());
        }
        let mark = match crate::authority::check_start(
            &mut tx,
            &run,
            &call,
            &s,
            pointer.is_some(),
            authority,
        )
        .await?
        {
            Ok(mark) => mark,
            Err(why) if authority.is_none() => {
                // Grant-bound direct call attempted without authority: deny immediately.
                crate::authority::deny_in(&mut tx, &run, &call, why).await?;
                tx.commit().await?;
                return Err(Error::Denied.into());
            }
            Err(why) => {
                tx.rollback().await?;
                return Ok(bot_core::grant::Granted::Refused(why));
            }
        };
        // Require the linked approval to be presented so callers cannot bypass consumption.
        crate::approvals::require_presented(&mut tx, pointer.as_ref(), s.approval.is_some())
            .await?;
        let mut verdict = "started".to_owned();
        let mut approved = None;
        if let Some((expected, reasons)) = &s.approval {
            crate::approvals::consume(&mut tx, at, &s.call, expected, reasons).await?;
            approved = Some(expected.nonce.0.clone());
            verdict = "started:approved".into();
        }
        sqlx::query(
            "update bot_calls set state = 'started', started_generation = $4, owner = $5, started_ms = bot_now_ms(),
                    updated_ms = bot_now_ms(), risk = $6, tool_revision = $7, approval = coalesce($8, approval),
                    grant_id = $9, grant_epoch = $10
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&s.call.0)
        .bind(at.generation.0 as i64)
        .bind(&s.owner)
        .bind(risk_str(s.risk))
        .bind(&s.tool_revision.0)
        .bind(approved)
        .bind(mark.grant.as_ref().map(|(id, _)| id.as_str()))
        .bind(mark.grant.as_ref().map(|(_, epoch)| *epoch))
        .execute(&mut *tx)
        .await?;
        let principal: String =
            sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_one(&mut *tx)
                .await?;
        let verdict = mark.verdict(&verdict);
        let mut entry = Entry::new(&principal, "call.start", &s.call.0, &verdict);
        entry.args = Some(to32(call.get("args_sha256"))?);
        audit::append(&mut tx, &run.profile, &entry).await?;
        tx.commit().await?;
        Ok(bot_core::grant::Granted::Started)
    }

    /// Settles a call: `Open -> Blocked`, `Started -> any`, or resolved `Held -> Done/Released/Blocked`.
    pub(crate) async fn call_finish(&self, f: FinishCall) -> R<()> {
        self.call_finish_with(f, None).await
    }

    /// Like `call_finish`, persisting an `ApprovalBlock` reason code on `Blocked` calls for API run lookup.
    pub(crate) async fn call_finish_with(
        &self,
        f: FinishCall,
        block: Option<bot_core::api::ApprovalBlock>,
    ) -> R<()> {
        if block.is_some() && !matches!(f.outcome, Outcome::Blocked) {
            return Err(Error::Conflict.into());
        }
        let at = &f.attempt;
        let result = match &f.outcome {
            Outcome::Done(payload) => Some(canonical(payload)?.1),
            Outcome::Held(reason) if !id_ok(reason) || reason.len() > 64 => return invalid(),
            _ => None,
        };
        let mut tx = self.pool.begin().await?;
        let run = fence_settle(&mut tx, at).await?;
        let row = lock_call(&mut tx, at, &f.call).await?;
        let phase = phase_of(&row)?;
        if matches!(f.outcome, Outcome::Released) && row.get::<bool, _>("confirmed_step") {
            return Err(Error::Conflict.into());
        }
        let allowed = match (&phase, &f.outcome) {
            (CallPhase::Open { .. }, Outcome::Blocked) => true,
            (CallPhase::Started { generation, .. }, _) => generation.0 <= at.generation.0,
            (
                CallPhase::Held {
                    resolution: Some(Resolution::Executed { .. }),
                    ..
                },
                Outcome::Done(_),
            ) => true,
            (
                CallPhase::Held {
                    resolution: Some(Resolution::NotExecuted),
                    ..
                },
                Outcome::Released | Outcome::Blocked,
            ) => true,
            _ => false,
        };
        if !allowed {
            return Err(Error::Conflict.into());
        }
        let (state, reason, label, verdict) = match &f.outcome {
            Outcome::Done(_) => ("done", None, "call.done", "done".to_owned()),
            Outcome::Released => ("open", None, "call.released", "released".to_owned()),
            Outcome::Held(reason) => (
                "held",
                Some(reason.as_str()),
                "call.held",
                format!("held:{reason}"),
            ),
            Outcome::Blocked => (
                "denied",
                block.map(bot_core::api::ApprovalBlock::code),
                "call.blocked",
                block.map_or_else(|| "blocked".to_owned(), |b| format!("blocked:{}", b.code())),
            ),
        };
        // Clear start/resolution markers on `Released` (`open`) so the call can restart; retain them on `held` for recovery.
        sqlx::query(
            "update bot_calls set state = $4, result = $5, reason = $6, updated_ms = bot_now_ms(),
                    finished_ms = case when $4 in ('done', 'denied') then bot_now_ms() else null end,
                    approval = case when $4 = 'open' then null else approval end,
                    started_generation = case when $4 = 'open' then null else started_generation end,
                    owner = case when $4 = 'open' then null else owner end,
                    resolution = case when $4 in ('open', 'held') and $7 then null else resolution end,
                    receipt = case when $4 in ('open', 'held') and $7 then null else receipt end
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&f.call.0)
        .bind(state)
        .bind(result)
        .bind(reason)
        .bind(!matches!(phase, CallPhase::Held { .. }) || state == "open")
        .execute(&mut *tx)
        .await?;
        if state == "held" {
            // Move any `started` effect steps to `held` atomically so they appear individually in the operator hold queue.
            sqlx::query(
                "update bot_effects set state = 'held', reason = coalesce(reason, $4), updated_ms = bot_now_ms()
                 where profile = $1 and run = $2 and call = $3 and state = 'started'",
            )
            .bind(&at.profile.0)
            .bind(&at.run.0)
            .bind(&f.call.0)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
        }
        let principal: String =
            sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_one(&mut *tx)
                .await?;
        if state == "held" {
            park(&mut tx, &run, &Wait::Hold).await?;
        }
        let mut entry = OwnedEntry::new(&run.profile, &principal, label, &f.call.0, &verdict);
        entry.args = Some(to32(row.get("args_sha256"))?);
        let mut audits = vec![entry];
        if state == "held" {
            audits.push(OwnedEntry::new(
                &run.profile,
                "system",
                "run.wait",
                &run.id,
                format!("hold:{}", f.call.0),
            ));
        }
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(())
    }
}
