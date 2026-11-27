//! Approval ledger: decision, superseding, expiration, and single-use consumption with generation fencing.
//!
//! Lock order: run -> call -> approval -> audit head (always last).
//! `expire_approvals` commits one transaction per run in this same order to avoid deadlocks with concurrent decisions.

use crate::{
    audit::{self, Entry, OwnedEntry},
    calls::{lock_call, risk_from},
    codec::*,
    db::{R, Store, Tx, ms_to_time},
};
use bot_core::{
    id::*,
    ledger::*,
    port::Error,
    state::{Approval, RunContext},
};
use sqlx::{Row, postgres::PgRow};

fn state_str(state: ApprovalState) -> &'static str {
    match state {
        ApprovalState::Pending => "pending",
        ApprovalState::Approved => "approved",
        ApprovalState::Consumed => "consumed",
        ApprovalState::Denied => "denied",
        ApprovalState::Expired => "expired",
        ApprovalState::Superseded => "superseded",
        ApprovalState::Revoked => "revoked",
    }
}

pub(crate) fn state_of(s: &str) -> R<ApprovalState> {
    Ok(match s {
        "pending" => ApprovalState::Pending,
        "approved" => ApprovalState::Approved,
        "consumed" => ApprovalState::Consumed,
        "denied" => ApprovalState::Denied,
        "expired" => ApprovalState::Expired,
        "superseded" => ApprovalState::Superseded,
        "revoked" => ApprovalState::Revoked,
        _ => return invalid(),
    })
}

pub(crate) fn approval_of(row: &PgRow) -> R<Approval> {
    Ok(Approval {
        nonce: ApprovalId(row.get("nonce")),
        attempt: Attempt {
            profile: ProfileId(row.get("profile")),
            run: RunId(row.get("run")),
            claim: None,
            generation: Generation(row.get::<i64, _>("generation") as u64),
        },
        call: CallId(row.get("call")),
        requester: PrincipalId(row.get("requester")),
        approver: PrincipalId(row.get("approver")),
        risk: risk_from(&row.get::<String, _>("risk"))?,
        policy_revision: Revision(row.get("policy_revision")),
        tool: ToolId(row.get("tool")),
        tool_revision: Revision(row.get("tool_revision")),
        args_sha256: to32(row.get("args_sha256"))?,
        expires_at: ms_to_time(row.get("expires_ms")),
    })
}

/// Checks whether a stored approval matches the recomputed binding fields (excluding nonce, expiry, and transient claim token).
fn same_binding(stored: &Approval, expected: &Approval) -> bool {
    let (a, b) = (stored, expected);
    (&a.attempt.profile, &a.attempt.run, a.attempt.generation)
        == (&b.attempt.profile, &b.attempt.run, b.attempt.generation)
        && a.call == b.call
        && a.requester == b.requester
        && a.approver == b.approver
        && a.risk == b.risk
        && a.policy_revision == b.policy_revision
        && a.tool == b.tool
        && a.tool_revision == b.tool_revision
        && a.args_sha256 == b.args_sha256
}

/// Moves an approval-waiting run back to `ready` in the same generation once no pending approvals remain.
pub(crate) async fn wake_waiters(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<bool> {
    let woke = sqlx::query(
        "update bot_runs set state = 'ready', wait_kind = null, keep_generation = true, updated_ms = bot_now_ms()
         where profile = $1 and id = $2 and state = 'waiting' and wait_kind = 'approval'
           and not exists (select 1 from bot_approvals a where a.profile = $1 and a.run = $2 and a.state = 'pending')",
    )
    .bind(profile)
    .bind(run)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(woke == 1)
}

/// Locks the approval's run first (preserving `run -> approval` order) and rejects stale generations or cancelled runs.
async fn lock_run_for(tx: &mut Tx<'_>, approval: &PgRow) -> R<()> {
    let run = sqlx::query(
        "select generation, state, cancel_requested from bot_runs where profile = $1 and id = $2 for update",
    )
    .bind(approval.get::<String, _>("profile"))
    .bind(approval.get::<String, _>("run"))
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::Stale)?;
    let current = run.get::<i64, _>("generation") == approval.get::<i64, _>("generation");
    let live =
        !run.get::<bool, _>("cancel_requested") && run.get::<String, _>("state") != "cancelled";
    if current && live {
        Ok(())
    } else {
        Err(Error::Stale.into())
    }
}

async fn peek(tx: &mut Tx<'_>, nonce: &str) -> R<PgRow> {
    sqlx::query(&format!(
        "select {} from bot_approvals where nonce = $1",
        crate::cols::APPROVALS
    ))
    .bind(nonce)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Missing.into())
}

pub(crate) async fn lock_approval(tx: &mut Tx<'_>, nonce: &str) -> R<PgRow> {
    sqlx::query(&format!(
        "select {} from bot_approvals where nonce = $1 for update",
        crate::cols::APPROVALS
    ))
    .bind(nonce)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Missing.into())
}

/// Blocks call start if the call still points to a non-superseded approval that was not presented.
pub(crate) async fn require_presented(
    tx: &mut Tx<'_>,
    pointer: Option<&ApprovalId>,
    presented: bool,
) -> R<()> {
    let Some(nonce) = pointer.filter(|_| !presented) else {
        return Ok(());
    };
    let state: Option<String> =
        sqlx::query_scalar("select state from bot_approvals where nonce = $1")
            .bind(&nonce.0)
            .fetch_optional(&mut **tx)
            .await?;
    if state.as_deref() == Some("superseded") {
        Ok(())
    } else {
        Err(Error::Denied.into())
    }
}

/// Consumes an approved record inside the caller's `start` transaction after verifying binding, reasons, expiry, and call linkage.
pub(crate) async fn consume(
    tx: &mut Tx<'_>,
    at: &Attempt,
    call: &CallId,
    expected: &Approval,
    reasons: &[String],
) -> R<()> {
    let row = lock_approval(tx, &expected.nonce.0).await?;
    if row.get::<String, _>("profile") != at.profile.0
        || row.get::<String, _>("run") != at.run.0
        || row.get::<String, _>("call") != call.0
    {
        return Err(Error::Denied.into());
    }
    match state_of(&row.get::<String, _>("state"))? {
        ApprovalState::Approved => {}
        ApprovalState::Consumed => return Err(Error::Conflict.into()),
        ApprovalState::Pending | ApprovalState::Denied | ApprovalState::Revoked => {
            return Err(Error::Denied.into());
        }
        ApprovalState::Expired | ApprovalState::Superseded => return Err(Error::Stale.into()),
    }
    let now: i64 = sqlx::query_scalar("select bot_now_ms()")
        .fetch_one(&mut **tx)
        .await?;
    let stored_reasons: Vec<String> = row.get("reasons");
    if now >= row.get::<i64, _>("expires_ms")
        || !same_binding(&approval_of(&row)?, expected)
        || stored_reasons != reasons
    {
        return Err(Error::Stale.into());
    }
    // Cross-check the stored call row against the stored approval row, not caller-supplied arguments.
    let recorded = sqlx::query(
        "select tool, args_sha256, approval from bot_calls where profile = $1 and run = $2 and call = $3",
    )
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .bind(&call.0)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::Missing)?;
    if recorded.get::<Option<String>, _>("approval").as_deref() != Some(expected.nonce.0.as_str()) {
        return Err(Error::Denied.into());
    }
    if recorded.get::<String, _>("tool") != row.get::<String, _>("tool")
        || recorded.get::<Vec<u8>, _>("args_sha256") != row.get::<Vec<u8>, _>("args_sha256")
    {
        return Err(Error::Conflict.into());
    }
    sqlx::query("update bot_approvals set state = 'consumed' where nonce = $1")
        .bind(&expected.nonce.0)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

impl Store {
    /// Read-only run context for the approver at the current generation, omitting the worker claim token.
    pub(crate) async fn approval_context(
        &self,
        nonce: &ApprovalId,
        actor: &PrincipalId,
    ) -> R<RunContext> {
        let mut tx = self.pool.begin().await?;
        let seen = peek(&mut tx, &nonce.0).await?;
        if seen.get::<String, _>("approver") != actor.0 {
            return Err(Error::Denied.into());
        }
        let (profile, run): (String, String) = (seen.get("profile"), seen.get("run"));
        let row = sqlx::query("select generation, state, cancel_requested from bot_runs where profile = $1 and id = $2")
            .bind(&profile)
            .bind(&run)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::Stale)?;
        let current = row.get::<i64, _>("generation") == seen.get::<i64, _>("generation");
        let live =
            !row.get::<bool, _>("cancel_requested") && row.get::<String, _>("state") != "cancelled";
        if !(current && live) {
            return Err(Error::Stale.into());
        }
        let mut context = crate::queue::load(&mut tx, &profile, &run).await?;
        context.attempt.claim = None;
        tx.rollback().await?;
        Ok(context)
    }

    pub(crate) async fn approval_get(&self, nonce: &ApprovalId) -> R<StoredApproval> {
        let row = sqlx::query(&format!(
            "select {} from bot_approvals where nonce = $1",
            crate::cols::APPROVALS
        ))
        .bind(&nonce.0)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::Missing)?;
        Ok(StoredApproval {
            approval: approval_of(&row)?,
            reasons: row.get("reasons"),
            card: bot_core::command::JsonPayload(row.get("card")),
            state: state_of(&row.get::<String, _>("state"))?,
            // Present only after an owner-DM card job has confirmed `posted` (used for click verification).
            delivery: self.cards_receipt(nonce).await?,
        })
    }

    /// Supersedes a pending/approved record still pointed to by its call; idempotent if already superseded, `Conflict` if consumed.
    pub(crate) async fn approval_supersede(
        &self,
        nonce: &ApprovalId,
        actor: &PrincipalId,
    ) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let seen = peek(&mut tx, &nonce.0).await?;
        if seen.get::<String, _>("approver") != actor.0 {
            return Err(Error::Denied.into());
        }
        if state_of(&seen.get::<String, _>("state"))? == ApprovalState::Superseded {
            tx.rollback().await?;
            return Ok(());
        }
        // Lock order: run -> call -> approval -> audit head.
        lock_run_for(&mut tx, &seen).await?;
        let call_at = Attempt {
            profile: ProfileId(seen.get("profile")),
            run: RunId(seen.get("run")),
            claim: None,
            generation: Generation(seen.get::<i64, _>("generation") as u64),
        };
        let call = lock_call(&mut tx, &call_at, &CallId(seen.get("call"))).await?;
        let row = lock_approval(&mut tx, &nonce.0).await?;
        let pointed = matches!(crate::calls::record_of(&call)?.phase,
            bot_core::ledger::CallPhase::Open { approval: Some(ref n) } if n == nonce);
        match state_of(&row.get::<String, _>("state"))? {
            ApprovalState::Superseded => {
                tx.rollback().await?;
                return Ok(());
            }
            ApprovalState::Pending | ApprovalState::Approved if pointed => {}
            _ => return Err(Error::Conflict.into()),
        }
        sqlx::query("update bot_approvals set state = 'superseded' where nonce = $1")
            .bind(&nonce.0)
            .execute(&mut *tx)
            .await?;
        let profile: String = row.get("profile");
        wake_waiters(&mut tx, &profile, &row.get::<String, _>("run")).await?;
        audit::append(
            &mut tx,
            &profile,
            &Entry::new(&actor.0, "approval.supersede", &nonce.0, "superseded"),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Applies an approver decision (`Pending -> Approved|Denied`, `Approved + deny -> Revoked`) and wakes waiting runs atomically.
    pub(crate) async fn approval_decide(&self, d: Decision) -> R<ApprovalState> {
        let mut tx = self.begin_authorized().await?;
        let seen = peek(&mut tx, &d.nonce.0).await?;
        if seen.get::<String, _>("approver") != d.actor.0 {
            return Err(Error::Denied.into());
        }
        lock_run_for(&mut tx, &seen).await?;
        let row = lock_approval(&mut tx, &d.nonce.0).await?;
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        if now >= row.get::<i64, _>("expires_ms") {
            return Err(Error::Stale.into());
        }
        // Owner-DM cards remain closed to decisions until delivery is confirmed (`posted`).
        let job: Option<String> =
            sqlx::query_scalar("select state from bot_card_jobs where nonce = $1 for share")
                .bind(&d.nonce.0)
                .fetch_optional(&mut *tx)
                .await?;
        if job.is_some_and(|state| state != "posted") {
            return Err(Error::Conflict.into());
        }
        let next = match (state_of(&row.get::<String, _>("state"))?, d.allow) {
            (ApprovalState::Pending, true) => ApprovalState::Approved,
            (ApprovalState::Pending, false) => ApprovalState::Denied,
            (ApprovalState::Approved, false) => ApprovalState::Revoked,
            _ => return Err(Error::Conflict.into()),
        };
        sqlx::query("update bot_approvals set state = $2, decided_by = $3, decided_ms = bot_now_ms() where nonce = $1")
            .bind(&d.nonce.0)
            .bind(state_str(next))
            .bind(&d.actor.0)
            .execute(&mut *tx)
            .await?;
        let profile: String = row.get("profile");
        wake_waiters(&mut tx, &profile, &row.get::<String, _>("run")).await?;
        let mut entry = Entry::new(&d.actor.0, "approval.decide", &d.nonce.0, state_str(next));
        entry.args = Some(to32(row.get("args_sha256"))?);
        audit::append(&mut tx, &profile, &entry).await?;
        tx.commit().await?;
        Ok(next)
    }

    /// Expires overdue approvals per run so each transaction preserves `run -> approval -> audit head` lock order.
    /// Candidate runs are scanned unlocked and re-verified under lock; stops on first error after prior runs commit.
    pub(crate) async fn expire_approvals(&self) -> R<()> {
        let due = sqlx::query(
            "select profile, run from bot_approvals
             where state in ('pending', 'approved') and expires_ms <= (select bot_now_ms())
             group by profile, run order by profile, run",
        )
        .fetch_all(&self.pool)
        .await?;
        for row in &due {
            let (profile, run): (String, String) = (row.get("profile"), row.get("run"));
            self.expire_run_approvals(&profile, &run).await?;
        }
        Ok(())
    }

    async fn expire_run_approvals(&self, profile: &str, run: &str) -> R<()> {
        let mut tx = self.pool.begin().await?;
        let locked =
            sqlx::query("select 1 from bot_runs where profile = $1 and id = $2 for update")
                .bind(profile)
                .bind(run)
                .fetch_optional(&mut *tx)
                .await?;
        if locked.is_none() {
            return Ok(()); // Run already deleted (approvals cascade).
        }
        let mut gone: Vec<String> = sqlx::query_scalar(
            "update bot_approvals set state = 'expired'
             where profile = $1 and run = $2 and state in ('pending', 'approved') and expires_ms <= (select bot_now_ms())
             returning nonce",
        )
        .bind(profile)
        .bind(run)
        .fetch_all(&mut *tx)
        .await?;
        if gone.is_empty() {
            return Ok(()); // Decided, superseded, or consumed while acquiring the lock.
        }
        gone.sort();
        wake_waiters(&mut tx, profile, run).await?;
        let audits: Vec<OwnedEntry> = gone
            .into_iter()
            .map(|nonce| OwnedEntry::new(profile, "system", "approval.expire", nonce, "expired"))
            .collect();
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Supersedes approvals from generations `<= generation` on lease handover; caller holds the run lock.
pub(crate) async fn supersede_generation_into(
    tx: &mut Tx<'_>,
    profile: &str,
    run: &str,
    generation: i64,
    audits: &mut Vec<OwnedEntry>,
) -> R<()> {
    let mut gone: Vec<String> = sqlx::query_scalar(
        "update bot_approvals set state = 'superseded'
         where profile = $1 and run = $2 and generation <= $3 and state in ('pending', 'approved') returning nonce",
    )
    .bind(profile)
    .bind(run)
    .bind(generation)
    .fetch_all(&mut **tx)
    .await?;
    gone.sort();
    for nonce in gone {
        audits.push(OwnedEntry::new(
            profile,
            "system",
            "approval.supersede",
            nonce,
            "owner_lost",
        ));
    }
    Ok(())
}

/// Supersedes open approvals when a run terminates; caller holds the run lock.
pub(crate) async fn close_for_run(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<()> {
    sqlx::query(
        "update bot_approvals set state = 'superseded' where profile = $1 and run = $2 and state in ('pending', 'approved')",
    )
    .bind(profile)
    .bind(run)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
