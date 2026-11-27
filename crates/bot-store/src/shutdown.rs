//! Graceful worker shutdown release: returns a leased run to `ready` without spending an attempt once host cleanup completes.

use crate::{
    approvals,
    audit::{self, OwnedEntry},
    db::{R, Store},
    finish::lock_leased,
};
use bot_core::{id::Attempt, port::Error};
use sqlx::Row;

impl Store {
    /// Returns an active lease to `ready` without consuming an attempt, bumping generation (with `keep_generation`)
    /// and superseding old approvals. Rejects cancelled/private runs (`Denied`) and stale leases (`Stale`); works without an active-set pin.
    pub(crate) async fn release_for_shutdown_run(&self, at: &Attempt) -> R<()> {
        let mut tx = self.pool.begin().await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private || run.cancel_requested {
            return Err(Error::Denied.into());
        }
        sqlx::query(
            "update bot_runs set state = 'ready', lease_until_ms = null, lease_owner = null,
                    generation = generation + 1, keep_generation = true, updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .execute(&mut *tx)
        .await?;
        // Supersede old-generation approvals while leaving started calls for broker recovery on the next claim.
        let mut audits = Vec::new();
        approvals::supersede_generation_into(
            &mut tx,
            &run.profile,
            &run.id,
            at.generation.0 as i64,
            &mut audits,
        )
        .await?;
        audits.push(OwnedEntry::new(
            &run.profile,
            "system",
            "run.release",
            &run.id,
            "shutdown",
        ));
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Lists owner tokens for still-started tool calls in this generation so shutdown cleanup can reap them.
    pub async fn started_owners(&self, at: &Attempt) -> Result<Vec<String>, Error> {
        sqlx::query(
            "select owner from bot_calls where profile = $1 and run = $2 and state = 'started' and started_generation = $3
             and owner is not null order by call",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(at.generation.0 as i64)
        .fetch_all(&self.pool)
        .await
        .map(|rows| rows.into_iter().map(|row| row.get("owner")).collect())
        .map_err(|e| crate::db::Fail::from(e).into_core())
    }
}
