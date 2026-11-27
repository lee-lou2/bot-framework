//! Atomic host active-set publication (CAS) and per-profile active-revision guard.

use crate::memory::{check_seed, seed_audit, seed_in};
use crate::{
    audit::{self, OwnedEntry},
    codec::*,
    db::{R, Store},
    schedule::{sync_in, validate_set},
};
use bot_core::{
    command::{MemorySeed, ScheduleSet, TriggerSpec},
    grant::{GrantId, GrantSpec},
    id::*,
    port::Error,
};

use crate::db::Tx;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy)]
pub(crate) struct Extra<'a> {
    pub prepared: Option<&'a BTreeSet<(ProfileId, Revision)>>,
    pub grants: &'a [GrantSpec],
    pub sync_grants: bool,
}

impl Store {
    pub(crate) async fn current_active(&self) -> R<Option<Revision>> {
        let current: Option<String> =
            sqlx::query_scalar("select revision from bot_active where singleton")
                .fetch_optional(&self.pool)
                .await?;
        Ok(current.filter(|r| !r.is_empty()).map(Revision))
    }

    /// Atomically publishes the active set, memory seeds, static schedule segments, grants, and audit via CAS.
    /// All schedule segment boundaries share this transaction's DB timestamp, closing dropped profiles' open segments.
    pub(crate) async fn replace_active(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<TriggerSpec>>,
        extra: Extra<'_>,
    ) -> R<Vec<(GrantId, GrantSpec)>> {
        let Extra {
            prepared,
            grants,
            sync_grants,
        } = extra;
        ids_ok(&[&next.0])?;
        if profiles.iter().any(|(p, r)| !id_ok(&p.0) || !id_ok(&r.0)) {
            return invalid();
        }
        for seed in &seeds {
            if check_seed(seed).is_err() || !profiles.contains_key(&seed.profile) {
                return invalid();
            }
        }
        if schedules.keys().any(|p| !profiles.contains_key(p)) {
            return invalid();
        }
        let sets: Vec<ScheduleSet> = profiles
            .iter()
            .map(|(profile, revision)| ScheduleSet {
                profile: profile.clone(),
                revision: revision.clone(),
                triggers: schedules.get(profile).cloned().unwrap_or_default(),
            })
            .collect();
        for set in &sets {
            validate_set(set)?;
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into bot_active (singleton, revision) values (true, '') on conflict do nothing",
        )
        .execute(&mut *tx)
        .await?;
        let current: String =
            sqlx::query_scalar("select revision from bot_active where singleton for update")
                .fetch_one(&mut *tx)
                .await?;
        // Empty revision means no active set has been published yet.
        let current = (!current.is_empty()).then_some(current);
        if current != expected.map(|r| r.0) {
            return Err(Error::Conflict.into());
        }
        // Under the `bot_active` lock, verify every pinned revision is in `prepared`; otherwise fail `Stale` without writes.
        if let Some(prepared) = prepared {
            let required = crate::profile_bundles::pinned_in(&mut *tx, &profiles).await?;
            if !required.is_subset(prepared) {
                return Err(Error::Stale.into());
            }
        }
        let removed: Vec<String> = sqlx::query_scalar("select profile from bot_active_profiles")
            .fetch_all(&mut *tx)
            .await?;
        sqlx::query("delete from bot_active_profiles")
            .execute(&mut *tx)
            .await?;
        for (profile, revision) in &profiles {
            sqlx::query("insert into bot_active_profiles (profile, revision) values ($1, $2)")
                .bind(&profile.0)
                .bind(&revision.0)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "update bot_active set revision = $1, updated_ms = bot_now_ms() where singleton",
        )
        .bind(&next.0)
        .execute(&mut *tx)
        .await?;
        // Sync static schedules before acquiring the audit head lock (lock order).
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        for set in &sets {
            sync_in(&mut tx, set, now).await?;
        }
        let keep: Vec<String> = profiles.keys().map(|p| p.0.clone()).collect();
        let dropped: Vec<String> = sqlx::query_scalar(
            "select distinct profile from bot_schedule_segments where until_ms is null and not (profile = any($1))",
        )
        .bind(&keep)
        .fetch_all(&mut *tx)
        .await?;
        for profile in dropped {
            let empty = ScheduleSet {
                profile: ProfileId(profile),
                revision: Revision(String::new()),
                triggers: Vec::new(),
            };
            sync_in(&mut tx, &empty, now).await?;
        }
        // Sync pre-approval grants only when `sync_grants` is set; live skill/MCP edits leave existing grants untouched.
        let (issued, grant_notes) = if sync_grants {
            crate::grants::sync_in(&mut tx, grants, &profiles).await?
        } else {
            (Vec::new(), Vec::new())
        };
        // Insert memory seeds before locking the audit head (lock order); audit only newly inserted rows.
        let mut fresh = Vec::new();
        for seed in &seeds {
            if seed_in(&mut tx, seed).await? {
                fresh.push(seed);
            }
        }
        let mut audits = Vec::new();
        for (profile, revision) in &profiles {
            audits.push(OwnedEntry::new(
                &profile.0,
                "system",
                "active.replace",
                &next.0,
                &revision.0,
            ));
        }
        for seed in fresh {
            audits.push(OwnedEntry::from_entry(&seed.profile.0, &seed_audit(seed)));
        }
        for note in grant_notes {
            audits.push(note.into_audit());
        }
        for gone in removed
            .iter()
            .filter(|p| !profiles.contains_key(&ProfileId((*p).clone())))
        {
            audits.push(OwnedEntry::new(
                gone,
                "system",
                "active.replace",
                &next.0,
                "removed",
            ));
        }
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(issued)
    }
}

/// Serializes authorization checks against `replace_active` via `bot_active FOR SHARE`, then checks the profile's active revision.
/// Lock order: `bot_active` -> (root run) -> run -> call/approval -> audit head.
pub(crate) async fn guard_active(tx: &mut Tx<'_>, profile: &str, checked: &Revision) -> R<()> {
    sqlx::query("select 1 from bot_active where singleton for share")
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::Stale)?;
    let active: Option<String> =
        sqlx::query_scalar("select revision from bot_active_profiles where profile = $1")
            .bind(profile)
            .fetch_optional(&mut **tx)
            .await?;
    if active.as_deref() == Some(checked.0.as_str()) {
        Ok(())
    } else {
        Err(Error::Stale.into())
    }
}
