//! Read-only effect step lookup (`EffectJournal::lookup`) verifying lease, bound tool, and args digest.
//!
//! Never writes steps, receipts, or audit rows, and never consumes pending hold resolutions.
//! Validation order: input (`Invalid`) -> lease fence -> bound tool (`Denied`, hiding missing vs mismatched calls) -> step (`None`) -> args digest (`Conflict`).

use crate::{
    calls::fence,
    codec::ids_ok,
    db::{R, Store},
    effects::{args_digest, status_of},
};
use bot_core::{
    command::{EffectRequest, EffectStatus},
    id::ToolId,
    port::Error,
};
use sqlx::Row;

impl Store {
    pub(crate) async fn effect_lookup(
        &self,
        r: EffectRequest,
        tool: &ToolId,
    ) -> R<Option<EffectStatus>> {
        ids_ok(&[&r.call.0, &r.step, &tool.0])?;
        let digest = args_digest(&r.args)?;
        let at = &r.attempt;
        let mut tx = self.pool.begin().await?;
        fence(&mut tx, at).await?;
        let bound: Option<String> = sqlx::query_scalar(
            "select tool from bot_calls where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&r.call.0)
        .fetch_optional(&mut *tx)
        .await?;
        if bound.as_deref() != Some(tool.0.as_str()) {
            return Err(Error::Denied.into());
        }
        let row = sqlx::query(
            &format!("select {} from bot_effects where profile = $1 and run = $2 and call = $3 and step = $4", crate::cols::EFFECTS),
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&r.call.0)
        .bind(&r.step)
        .fetch_optional(&mut *tx)
        .await?;
        tx.rollback().await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.get::<Vec<u8>, _>("args_sha256") != digest {
            return Err(Error::Conflict.into());
        }
        status_of(&row).map(Some)
    }
}
