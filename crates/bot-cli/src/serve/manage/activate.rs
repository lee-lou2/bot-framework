//! Validates disk edits, stores bundles, CAS-commits active revision, and swaps live state.
use super::{LiveManager, yaml_edit::DiskTxn};
use crate::{
    host, install,
    load::check_entrances,
    serve::{Publication, restore, runner::refuse_required_state, seed},
};
use bot_core::{
    id::{ProfileId, Revision},
    manage::ManageError,
    port::Error,
    profile::ActiveRevisions,
};
use bot_profile::Limits;

const COMMIT_ATTEMPTS: usize = 4;

fn reject(code: &'static str, message: impl Into<String>) -> ManageError {
    ManageError::Rejected {
        code,
        message: message.into(),
    }
}

impl LiveManager {
    /// Validates staged disk changes and activates them immediately; dropping `txn` on error rolls back disk edits.
    pub(super) async fn activate_disk(
        &self,
        profile: &ProfileId,
        txn: DiskTxn,
    ) -> Result<Revision, ManageError> {
        let next_host = match &self.host_path {
            Some(path) => {
                let mut parsed =
                    host::load(path).map_err(|e| reject("invalid_host", e.to_string()))?;
                parsed.pin_uids().map_err(|e| reject("invalid_host", e))?;
                parsed
            }
            None => {
                let mut demo = host::demo(self.uid);
                demo.attach_demo_server(Some(&self.profiles_root));
                demo.pin_uids().map_err(|e| reject("invalid_host", e))?;
                demo
            }
        };

        let limits = Limits::default();
        let next_set = bot_profile::load(&self.profiles_root, &next_host.catalog, &limits)
            .map_err(|report| reject("validation_failed", report.to_string()))?;
        let new_profile_rev = next_set
            .profile(profile)
            .ok_or_else(|| reject("unknown_profile", "profile disappeared during validation"))?
            .revision
            .clone();

        check_entrances(next_set.active().clone(), &next_host)
            .map_err(|e| reject("invalid_entrances", e))?;
        let seed_problems: Vec<String> = next_set
            .active()
            .values()
            .flat_map(|snap| seed::problems(snap))
            .collect();
        if !seed_problems.is_empty() {
            return Err(reject("invalid_seeds", seed_problems.join("\n")));
        }
        refuse_required_state(&next_host.servers)
            .map_err(|e| reject("unsupported_server_state", e))?;
        let revisions = install::measure_all(&next_host.servers, &next_host.remote_servers)
            .map_err(|e| reject("server_measure_failed", e))?;
        let publication = Publication::of(&next_set, &next_host, &revisions)
            .map_err(|e| reject("publication_failed", e))?;

        let prepared_runner = self
            .live_runner
            .prepare_update(&next_host, &self.settings, self.uid)
            .await
            .map_err(|e| reject("runner_prepare_failed", e))?;

        let mut prepared = restore::prepare(
            self.raw_store.as_ref(),
            self.registry.as_ref(),
            &next_set,
            &publication.profiles,
            &limits,
        )
        .await
        .map_err(|_| ManageError::Unavailable("bundle_store_failed"))?;

        let expected = self
            .live_revision
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let mut committed = false;
        for attempt in 1..=COMMIT_ATTEMPTS {
            match self
                .raw_store
                .replace_live(
                    Some(expected.clone()),
                    publication.revision.clone(),
                    publication.profiles.clone(),
                    publication.seeds.clone(),
                    publication.schedules.clone(),
                    prepared.covered().clone(),
                )
                .await
            {
                Ok(()) => {
                    committed = true;
                    break;
                }
                Err(Error::Stale) if attempt < COMMIT_ATTEMPTS => {
                    restore::refresh(
                        self.raw_store.as_ref(),
                        self.registry.as_ref(),
                        &next_set,
                        &publication.profiles,
                        &limits,
                        &mut prepared,
                    )
                    .await
                    .map_err(|_| ManageError::Unavailable("bundle_refresh_failed"))?;
                }
                Err(Error::Conflict) => return Err(ManageError::Conflict),
                Err(_) => return Err(ManageError::Unavailable("active_replace_failed")),
            }
        }
        if !committed {
            return Err(ManageError::Unavailable("active_replace_stale"));
        }

        // After DB commit, finalize disk and swap all in-process state atomically with no await points.
        txn.commit();
        let (_applied, displaced) = self.registry.apply(next_set);
        {
            let mut held = self
                .held_snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            held.extend(displaced);
        }
        {
            let mut held_restored = self.held_restored.lock().unwrap_or_else(|e| e.into_inner());
            held_restored.push(prepared);
        }
        *self
            .live_revision
            .write()
            .unwrap_or_else(|e| e.into_inner()) = publication.revision;
        self.live_installed.update(&revisions);
        self.live_runner.apply(prepared_runner);
        *self.host.lock().unwrap_or_else(|e| e.into_inner()) = next_host;

        tracing::info!(
            profile = %profile.0,
            revision = %new_profile_rev.0,
            "activated live skill/MCP changes"
        );
        Ok(new_profile_rev)
    }
}
