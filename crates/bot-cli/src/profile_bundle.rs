//! Backfills archived profile bundles into PostgreSQL (`bot profile-bundle put`).
use crate::settings::Settings;
use bot_core::{id::Revision, port::Error, profile::ProfileBundles};
use bot_profile::{Limits, read_bundle, read_legacy_bundle};
use bot_store::{Config, Store};
use std::{collections::BTreeMap, path::Path};

pub async fn put(settings: &Settings, dir: &Path) -> Result<(), String> {
    let limits = Limits::default();
    let current = read_bundle(dir, &limits);
    let legacy = read_legacy_bundle(dir, &limits);
    let profile_id = match (&current, &legacy) {
        (Ok((id, _, _)), _) | (_, Ok((id, _, _))) => id.clone(),
        (Err(report), Err(_)) => {
            return Err(format!("failed to validate profile directory:\n{report}"));
        }
    };
    let url = settings
        .database_url
        .as_deref()
        .ok_or("BOT_DATABASE_URL is required (PostgreSQL connection URL)")?;
    let store = Store::connect(url, Config::default())
        .await
        .map_err(|_| "failed to connect to store")?;
    store
        .migrate()
        .await
        .map_err(|_| "failed to run store migrations")?;
    // Backfill every pinned revision (legacy V1 and/or current V2) produced by this directory;
    // if neither is pinned, store the current validated bundle and reject invalid unpinned dirs.
    let target = BTreeMap::from([(profile_id, Revision(String::new()))]);
    let pinned = store
        .pinned_revisions(&target)
        .await
        .map_err(|_| "failed to read pinned revisions from store")?;
    let mut targets = Vec::new();
    if let Ok((id, rev, bytes)) = legacy
        && pinned.contains(&(id.clone(), rev.clone()))
    {
        targets.push((id, rev, bytes));
    }
    match current {
        Ok((id, rev, bytes)) => {
            let duplicate = targets.iter().any(|(_, existing, _)| existing == &rev);
            if !duplicate && (targets.is_empty() || pinned.contains(&(id.clone(), rev.clone()))) {
                targets.push((id, rev, bytes));
            }
        }
        Err(report) if targets.is_empty() => {
            return Err(format!("failed to validate profile directory:\n{report}"));
        }
        Err(_) => {}
    }
    for (profile, revision, bundle) in targets {
        store
            .put_bundle(profile.clone(), revision.clone(), bundle)
            .await
            .map_err(|error| match error {
                Error::Conflict => {
                    "store already holds a different bundle under the same revision — store may be corrupted"
                }
                Error::Unavailable => "store is unavailable",
                _ => "store rejected the profile bundle",
            })?;
        println!(
            "Stored bundle for profile {} revision {} (unchanged if already present)",
            profile.0, revision.0
        );
    }
    Ok(())
}
