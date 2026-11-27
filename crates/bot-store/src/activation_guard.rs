//! Pins the active-set revision verified by gateway/admin handles and checks it in request transactions.
//! `begin_authorized` takes `FOR SHARE` on `bot_active` to serialize external requests against activation swaps.

use crate::db::{R, Store, Tx};
use bot_core::{id::Revision, port::Error};
use std::sync::{Arc, RwLock};

impl Store {
    /// Pins the active-set revision used by gateway/admin roles for authorization.
    pub fn for_activation(&self, revision: Revision) -> Self {
        self.for_live_activation(Arc::new(RwLock::new(revision)))
    }

    /// Binds this handle to a shared live active-set revision updated by hot skill/MCP activation.
    pub fn for_live_activation(&self, revision: Arc<RwLock<Revision>>) -> Self {
        Self {
            activation: Some(revision),
            ..self.clone()
        }
    }

    pub(crate) fn pinned_revision(&self) -> Option<Revision> {
        self.activation
            .as_ref()
            .map(|rev| rev.read().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// Defense-in-depth for read-only paths: fails with `Stale` if the pinned active set is no longer current.
    pub(crate) async fn check_pinned(&self, tx: &mut Tx<'_>) -> R<()> {
        if let Some(expected) = self.pinned_revision() {
            sqlx::query(
                "select 1 from bot_active where singleton and revision = $1 and revision <> ''",
            )
            .bind(&expected.0)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::Stale)?;
        }
        Ok(())
    }

    /// Serializes requests against activation swaps via `FOR SHARE` on `bot_active`. Unpinned handles pass through.
    pub(crate) async fn begin_authorized(&self) -> R<Tx<'_>> {
        let mut tx = self.pool.begin().await?;
        if let Some(expected) = self.pinned_revision() {
            sqlx::query("select 1 from bot_active where singleton and revision = $1 and revision <> '' for share")
                .bind(&expected.0)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Error::Stale)?;
        }
        Ok(tx)
    }
}
