//! Redeploy restore preparation: stores current profile bundles and restores pinned historical revisions before activation.
// Both storing and restoring complete before roles open or the active set changes; any failure leaves
// the active revision, in-flight runs, pending cards, and attempt counts untouched.

use bot_core::{
    id::{ProfileId, Revision},
    port::Error,
    profile::ProfileBundles,
};
use bot_profile::{Limits, ProblemKind, ProfileSet, Registry, Restored};
use std::collections::{BTreeMap, BTreeSet};

/// Holds strong references to restored snapshots (Registry keeps weak refs) and the covered revision set.
#[derive(Debug, Default)]
pub struct Prepared {
    held: Vec<Restored>,
    covered: BTreeSet<(ProfileId, Revision)>,
}

impl Prepared {
    pub fn covered(&self) -> &BTreeSet<(ProfileId, Revision)> {
        &self.covered
    }

    pub fn restored(&self) -> usize {
        self.held.iter().map(Restored::len).sum()
    }
}

/// Stores verified bundles for every active profile before publishing so future deploys can restore them.
pub async fn store_current(bundles: &dyn ProfileBundles, set: &ProfileSet) -> Result<(), String> {
    for (profile, snapshot) in set.active().iter() {
        let Some(bundle) = set.bundle(profile) else {
            return Err(format!(
                "profile {} has no verified bundle — active revision was not changed",
                profile.0
            ));
        };
        bundles
            .put_bundle(profile.clone(), snapshot.revision.clone(), bundle.to_vec())
            .await
            .map_err(|error| match error {
                Error::Conflict => format!(
                    "profile {} revision {}: a different bundle is already stored for this revision — the store may be corrupted; active revision was not changed",
                    profile.0, snapshot.revision.0
                ),
                Error::Unavailable => format!(
                    "profile {} revision {}: failed to store bundle (store unavailable) — active revision was not changed",
                    profile.0, snapshot.revision.0
                ),
                _ => format!(
                    "profile {} revision {}: store rejected bundle — active revision was not changed",
                    profile.0, snapshot.revision.0
                ),
            })?;
    }
    Ok(())
}

pub fn wanted(
    pinned: BTreeSet<(ProfileId, Revision)>,
    set: &ProfileSet,
) -> BTreeSet<(ProfileId, Revision)> {
    pinned
        .into_iter()
        .filter(|(profile, _)| set.active().contains_key(profile))
        .collect()
}

/// Stores current bundles and restores pinned historical revisions into `registry`; caller must keep `Prepared` alive.
pub async fn prepare(
    bundles: &dyn ProfileBundles,
    registry: &Registry,
    set: &ProfileSet,
    target: &BTreeMap<ProfileId, Revision>,
    limits: &Limits,
) -> Result<Prepared, String> {
    store_current(bundles, set).await?;
    let mut prepared = Prepared::default();
    refresh(bundles, registry, set, target, limits, &mut prepared).await?;
    Ok(prepared)
}

/// Re-reads pinned revisions and restores any newly required historical revisions discovered before commit.
pub async fn refresh(
    bundles: &dyn ProfileBundles,
    registry: &Registry,
    set: &ProfileSet,
    target: &BTreeMap<ProfileId, Revision>,
    limits: &Limits,
    prepared: &mut Prepared,
) -> Result<(), String> {
    let pinned = bundles.pinned_revisions(target).await.map_err(|error| {
        format!(
            "failed to read pinned revisions for in-flight work ({}) — active revision was not changed",
            match error {
                Error::Unavailable => "store unavailable",
                _ => "store rejected request",
            }
        )
    })?;
    let wanted = wanted(pinned, set);
    let restored = registry
        .restore(bundles, &wanted, limits)
        .await
        .map_err(|failure| {
            let damaged = failure
                .problems
                .iter()
                .any(|p| matches!(p.kind, ProblemKind::Unreadable(_)));
            let missing = failure
                .problems
                .iter()
                .any(|p| matches!(p.kind, ProblemKind::Missing));
            format!(
                "refusing to start: failed to restore historical revisions pinned by in-flight work — active revision, runs, pending cards, and attempt counts were not changed:\n{failure}\n\
                 In-flight runs, schedules, or approval cards still reference these revisions.{}{}",
                if missing {
                    " If you have the original profile directory, run `bot profile-bundle put <profile-dir>` and retry (only an exact profile ID and SHA-256 match fills that revision)."
                } else {
                    ""
                },
                if damaged {
                    " `bot profile-bundle put` will not overwrite corrupted bundles — restore the database from backup."
                } else {
                    ""
                }
            )
        })?;
    prepared.covered.extend(wanted);
    prepared.held.push(restored);
    Ok(())
}

pub async fn covers(
    bundles: &dyn ProfileBundles,
    set: &ProfileSet,
    target: &BTreeMap<ProfileId, Revision>,
    prepared: &Prepared,
) -> Result<bool, String> {
    let pinned = bundles.pinned_revisions(target).await.map_err(|_| {
        "failed to re-read pinned revisions for in-flight work (store unavailable)".to_owned()
    })?;
    Ok(wanted(pinned, set).is_subset(prepared.covered()))
}

#[cfg(test)]
mod tests;
