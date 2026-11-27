//! Immutable content-addressed profile bundles (`(profile, sha256_revision)`) and pinned-revision queries for restore.

use crate::{
    codec::*,
    db::{Fail, R, Store},
};
use bot_core::{
    id::{ProfileId, Revision},
    port::{Error, Reply},
    profile::ProfileBundles,
};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::{BTreeMap, BTreeSet};

fn revision_of(bundle: &[u8]) -> String {
    Sha256::digest(bundle)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Single-snapshot query of `(profile, revision)` pairs pinned by active work for the given profiles:
/// unfinished or resumable non-private dead runs, pending/approved approvals, active/unconsumed cron segments,
/// and active user schedules.
const PINNED: &str = "
    select profile, revision from bot_runs
     where profile = any($1) and (state in ('ready', 'leased', 'waiting', 'retry') or (state = 'dead' and not private))
    union
    select a.profile, r.revision from bot_approvals a
      join bot_runs r on r.profile = a.profile and r.id = a.run
     where a.profile = any($1) and a.state in ('pending', 'approved')
    union
    select s.profile, s.revision from bot_schedule_segments s
      join bot_schedule_cursors c on c.profile = s.profile and c.agent = s.agent and c.trigger = s.trigger
     where s.profile = any($1) and (s.until_ms is null or s.until_ms > c.cursor_ms)
    union
    select u.profile, r.revision from bot_user_schedules u
      join bot_runs r on r.profile = u.profile and r.id = u.origin_run
     where u.profile = any($1) and u.state = 'active'";

/// Returns pinned revisions unioned with `target` (usable on both pools and locked transactions).
pub(crate) async fn pinned_in<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    target: &BTreeMap<ProfileId, Revision>,
) -> R<BTreeSet<(ProfileId, Revision)>> {
    let ids: Vec<&str> = target.keys().map(|p| p.0.as_str()).collect();
    let rows = sqlx::query(PINNED).bind(&ids).fetch_all(executor).await?;
    let mut pinned: BTreeSet<(ProfileId, Revision)> = rows
        .iter()
        .map(|row| (ProfileId(row.get("profile")), Revision(row.get("revision"))))
        .collect();
    pinned.extend(target.iter().map(|(p, r)| (p.clone(), r.clone())));
    Ok(pinned)
}

impl Store {
    pub(crate) async fn bundle_put(
        &self,
        profile: &ProfileId,
        revision: &Revision,
        bundle: &[u8],
    ) -> R<()> {
        ids_ok(&[&profile.0, &revision.0])?;
        if revision.0 != revision_of(bundle) {
            return invalid();
        }
        sqlx::query(
            "insert into bot_profile_bundles (profile, revision, bundle) values ($1, $2, $3)
             on conflict (profile, revision) do nothing",
        )
        .bind(&profile.0)
        .bind(&revision.0)
        .bind(bundle)
        .execute(&self.pool)
        .await?;
        // Verify stored bytes match on conflict in DB to detect corruption without downloading the bundle.
        let same: bool = sqlx::query_scalar(
            "select bundle = $3 from bot_profile_bundles where profile = $1 and revision = $2",
        )
        .bind(&profile.0)
        .bind(&revision.0)
        .bind(bundle)
        .fetch_one(&self.pool)
        .await?;
        if !same {
            return Err(Fail::Core(Error::Conflict));
        }
        Ok(())
    }

    pub(crate) async fn bundle_get(
        &self,
        profile: &ProfileId,
        revision: &Revision,
    ) -> R<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar(
            "select bundle from bot_profile_bundles where profile = $1 and revision = $2",
        )
        .bind(&profile.0)
        .bind(&revision.0)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Immutable DB insertion timestamp (Unix ms) of the stored bundle, if present.
    pub(crate) async fn bundle_created_ms(
        &self,
        profile: &ProfileId,
        revision: &Revision,
    ) -> R<Option<i64>> {
        Ok(sqlx::query_scalar(
            "select created_ms from bot_profile_bundles where profile = $1 and revision = $2",
        )
        .bind(&profile.0)
        .bind(&revision.0)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub(crate) async fn pinned(
        &self,
        target: &BTreeMap<ProfileId, Revision>,
    ) -> R<BTreeSet<(ProfileId, Revision)>> {
        pinned_in(&self.pool, target).await
    }
}

impl ProfileBundles for Store {
    fn put_bundle(&self, profile: ProfileId, revision: Revision, bundle: Vec<u8>) -> Reply<'_, ()> {
        Box::pin(async move {
            self.bundle_put(&profile, &revision, &bundle)
                .await
                .map_err(Fail::into_core)
        })
    }

    fn bundle(&self, profile: &ProfileId, revision: &Revision) -> Reply<'_, Option<Vec<u8>>> {
        let (profile, revision) = (profile.clone(), revision.clone());
        Box::pin(async move {
            self.bundle_get(&profile, &revision)
                .await
                .map_err(Fail::into_core)
        })
    }

    fn created_ms(&self, profile: &ProfileId, revision: &Revision) -> Reply<'_, Option<i64>> {
        let (profile, revision) = (profile.clone(), revision.clone());
        Box::pin(async move {
            self.bundle_created_ms(&profile, &revision)
                .await
                .map_err(Fail::into_core)
        })
    }

    fn pinned_revisions<'a>(
        &'a self,
        target: &'a BTreeMap<ProfileId, Revision>,
    ) -> Reply<'a, BTreeSet<(ProfileId, Revision)>> {
        Box::pin(async move { self.pinned(target).await.map_err(Fail::into_core) })
    }
}
