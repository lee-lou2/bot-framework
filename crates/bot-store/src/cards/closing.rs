//! Owner-DM approval card sweep and failure closure (approval, audit, and origin notice in one commit).

use super::{CardKey, Locks, MAX_ATTEMPTS, can_open, key_of, lock_attempt, now_ms};
use crate::{
    approvals::wake_waiters,
    audit::{self, OwnedEntry},
    codec::to32,
    db::{Fail, R, Store, Tx},
    finish::{End, end_run_into, push_reply_into},
    replies,
    unsent::drop_draft,
};
use bot_core::{
    api::ApprovalBlock,
    cards::{APPROVAL_DM_UNAVAILABLE, DM_UNAVAILABLE_NOTICE, DM_UNCONFIRMED_NOTICE},
    command::JsonPayload,
    id::ApprovalId,
    port::Error,
    schedule::{DELIVER_CALL, DELIVER_TOOL},
};
use sqlx::{Row, postgres::PgRow};

const UNSENT_WORD: &str = "dm_unavailable";
/// Close `sending` jobs older than this as uncertain (`Held`) without auto-reposting.
const SENDING_MS: i64 = 120_000;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Closing {
    Unavailable,
    Held,
    /// Route drifted before posting (HTTP-origin only); stored as `ApprovalBlock::code`.
    Blocked(ApprovalBlock),
}

impl Closing {
    /// Fixed public notice for the originating channel; omitted for HTTP-origin route blocks.
    fn notice(self) -> Option<&'static str> {
        match self {
            Closing::Unavailable | Closing::Blocked(ApprovalBlock::ApprovalDmUnavailable) => {
                Some(DM_UNAVAILABLE_NOTICE)
            }
            Closing::Held => Some(DM_UNCONFIRMED_NOTICE),
            Closing::Blocked(_) => None,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Closing::Unavailable | Closing::Blocked(ApprovalBlock::ApprovalDmUnavailable) => {
                "unavailable"
            }
            Closing::Held => "held",
            Closing::Blocked(why) => why.code(),
        }
    }

    fn audit(self) -> (&'static str, &'static str) {
        match self {
            Closing::Blocked(why) if why != ApprovalBlock::ApprovalDmUnavailable => {
                ("approval.blocked", why.code())
            }
            _ => ("approval.dm_unavailable", APPROVAL_DM_UNAVAILABLE),
        }
    }
}

impl Store {
    /// Closes objectively stale jobs in bulk, then settles stuck `sending`/exhausted jobs under lock.
    pub(super) async fn card_sweep(&self) -> R<()> {
        sqlx::query(
            "update bot_card_jobs j
                set state = 'closed', closed_reason = 'stale', lease_until_ms = null, updated_ms = bot_now_ms()
               from bot_approvals a
              where a.nonce = j.nonce and j.state in ('staged', 'leased')
                and (a.state <> 'pending' or a.expires_ms <= bot_now_ms())",
        )
        .execute(&self.pool)
        .await?;
        let stuck = sqlx::query(
            "select nonce, attempts from bot_card_jobs
             where (state = 'sending' and sending_ms <= bot_now_ms() - $1)
                or (state = 'staged' and attempts >= $2)
                or (state = 'leased' and attempts >= $2 and lease_until_ms < bot_now_ms())
             order by created_ms, nonce limit 8",
        )
        .bind(SENDING_MS)
        .bind(MAX_ATTEMPTS)
        .fetch_all(&self.pool)
        .await?;
        for row in stuck {
            match self.sweep_card(&key_of(&row)).await {
                Ok(()) | Err(Fail::Core(Error::Stale | Error::Conflict | Error::Missing)) => {}
                Err(other) => return Err(other),
            }
        }
        Ok(())
    }

    async fn sweep_card(&self, key: &CardKey) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let locks = lock_attempt(&mut tx, key).await?;
        let now = now_ms(&mut tx).await?;
        let Some(closing) = sweep_reason(&locks.job, now) else {
            return Ok(());
        };
        self.close_card(tx, locks, &key.nonce, closing, now).await
    }

    pub(super) async fn card_close(&self, key: &CardKey, closing: Closing) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let locks = lock_attempt(&mut tx, key).await?;
        let state: String = locks.job.get("state");
        match state.as_str() {
            "closed" => return Ok(()),
            "leased" => {}
            "sending" if !matches!(closing, Closing::Blocked(_)) => {}
            _ => return Err(Error::Conflict.into()),
        }
        let now = now_ms(&mut tx).await?;
        if super::lease_expired(&locks.job, now) {
            return Err(Error::Stale.into());
        }
        self.close_card(tx, locks, &key.nonce, closing, now).await
    }

    /// Closes the card job and approval under lock, settling delivery drafts as unsent or waking waiting runs.
    pub(super) async fn close_card(
        &self,
        mut tx: Tx<'_>,
        locks: Locks,
        nonce: &ApprovalId,
        closing: Closing,
        now: i64,
    ) -> R<()> {
        if !can_open(&locks, now) {
            sqlx::query(
                "update bot_card_jobs set state = 'closed', closed_reason = 'stale', lease_until_ms = null, updated_ms = bot_now_ms() where nonce = $1",
            )
            .bind(&nonce.0)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(());
        }
        let (profile, run_id) = (locks.run.row.profile.clone(), locks.run.row.id.clone());
        // Closed by system delivery failure, not an owner decision.
        sqlx::query(
            "update bot_approvals set state = 'denied', decided_by = 'system', decided_ms = bot_now_ms() where nonce = $1",
        )
        .bind(&nonce.0)
        .execute(&mut *tx)
        .await?;
        let tool: String = locks.approval.get("tool");
        let drafted = if tool == DELIVER_TOOL {
            sqlx::query_scalar::<_, String>(
                "select state from bot_deliveries where profile = $1 and run = $2 for update",
            )
            .bind(&profile)
            .bind(&run_id)
            .fetch_optional(&mut *tx)
            .await?
            .as_deref()
                == Some("drafted")
        } else {
            false
        };
        let mut audits = Vec::new();
        if drafted && locks.run.row.parent.is_none() {
            // Delivery approval: drop the draft body and settle this occurrence as unsent.
            let receipt = serde_json::json!({ "status": APPROVAL_DM_UNAVAILABLE }).to_string();
            let set = locks.active.as_deref().ok_or(Error::Stale)?;
            drop_draft(
                &mut tx,
                (&profile, &run_id),
                locks.call_state.as_deref() == Some("open")
                    && locks.approval.get::<String, _>("call") == DELIVER_CALL,
                (UNSENT_WORD, &receipt, set),
                Some(&nonce.0),
            )
            .await?;
            end_run_into(
                &mut tx,
                &locks.run.row,
                End::Succeeded(&receipt),
                "system",
                &mut audits,
            )
            .await?;
        } else {
            // Other approvals: wake the run so the call observes the denied approval.
            wake_waiters(&mut tx, &profile, &run_id).await?;
        }
        sqlx::query(
            "update bot_card_jobs
                set state = 'closed', closed_reason = $2, lease_until_ms = null, updated_ms = bot_now_ms()
              where nonce = $1",
        )
        .bind(&nonce.0)
        .bind(closing.word())
        .execute(&mut *tx)
        .await?;
        // Post one fixed notice to the originating channel (no-op for HTTP-origin runs).
        if let Some(text) = closing.notice() {
            let notice = replies::answer(&JsonPayload(
                serde_json::json!({ "text": text }).to_string(),
            ))?;
            push_reply_into(&mut tx, &profile, &run_id, &notice, &mut audits).await?;
        }
        let (action, verdict) = closing.audit();
        let mut entry = OwnedEntry::new(&profile, "system", action, &nonce.0, verdict);
        entry.args = Some(to32(locks.approval.get("args_sha256"))?);
        audits.push(entry);
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(())
    }
}

fn sweep_reason(job: &PgRow, now: i64) -> Option<Closing> {
    let spent = job.get::<i32, _>("attempts") >= MAX_ATTEMPTS;
    match job.get::<String, _>("state").as_str() {
        "sending" if job.get::<i64, _>("sending_ms") <= now - SENDING_MS => Some(Closing::Held),
        "staged" if spent => Some(Closing::Unavailable),
        "leased" if spent && super::lease_expired(job, now) => Some(Closing::Unavailable),
        _ => None,
    }
}
