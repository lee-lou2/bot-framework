//! Tests for restore preparation using an in-memory `ProfileBundles` fake.

use super::*;
use bot_core::{
    port::Reply,
    profile::{ProfileBundles, ProfileCatalog},
};
use bot_profile::{HostCatalog, revision_of};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Mem {
    bundles: Mutex<BTreeMap<(ProfileId, Revision), Vec<u8>>>,
    pinned: Mutex<BTreeSet<(ProfileId, Revision)>>,
    put_error: Mutex<Option<Error>>,
    puts: Mutex<Vec<(ProfileId, Revision)>>,
    /// When set, `pinned_revisions` skips target filtering to verify CLI-side filtering.
    unfiltered: Mutex<bool>,
}

impl ProfileBundles for Mem {
    fn put_bundle(&self, profile: ProfileId, revision: Revision, bundle: Vec<u8>) -> Reply<'_, ()> {
        let failed = self.put_error.lock().unwrap().take();
        if failed.is_none() {
            self.puts
                .lock()
                .unwrap()
                .push((profile.clone(), revision.clone()));
            self.bundles
                .lock()
                .unwrap()
                .insert((profile, revision), bundle);
        }
        Box::pin(async move { failed.map_or(Ok(()), Err) })
    }

    fn bundle(&self, profile: &ProfileId, revision: &Revision) -> Reply<'_, Option<Vec<u8>>> {
        let found = self
            .bundles
            .lock()
            .unwrap()
            .get(&(profile.clone(), revision.clone()))
            .cloned();
        Box::pin(async move { Ok(found) })
    }

    fn created_ms(&self, _: &ProfileId, _: &Revision) -> Reply<'_, Option<i64>> {
        Box::pin(async { Ok(None) })
    }

    fn pinned_revisions<'a>(
        &'a self,
        target: &'a BTreeMap<ProfileId, Revision>,
    ) -> Reply<'a, BTreeSet<(ProfileId, Revision)>> {
        let all = *self.unfiltered.lock().unwrap();
        let mut pinned: BTreeSet<(ProfileId, Revision)> = self
            .pinned
            .lock()
            .unwrap()
            .iter()
            .filter(|(profile, _)| all || target.contains_key(profile))
            .cloned()
            .collect();
        pinned.extend(target.iter().map(|(p, r)| (p.clone(), r.clone())));
        Box::pin(async move { Ok(pinned) })
    }
}

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles")
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn deploy(registry: &Registry, root: &Path, note: &str) -> Arc<ProfileSet> {
    let dir = root.join("personal");
    let _ = std::fs::remove_dir_all(&dir);
    copy(&examples().join("personal"), &dir);
    let agent = dir.join("agents/main/AGENT.md");
    let mut text = std::fs::read_to_string(&agent).unwrap();
    text.push_str(note);
    std::fs::write(&agent, text).unwrap();
    registry
        .reload(root, &HostCatalog::demo(), &Limits::default())
        .unwrap_or_else(|report| panic!("{report}"))
}

fn target(set: &ProfileSet) -> BTreeMap<ProfileId, Revision> {
    set.active()
        .iter()
        .map(|(id, snapshot)| (id.clone(), snapshot.revision.clone()))
        .collect()
}

fn personal() -> ProfileId {
    ProfileId("personal".into())
}

fn assert_any(message: &str, needles: &[&str]) {
    let lower = message.to_ascii_lowercase();
    assert!(
        needles.iter().any(|n| lower.contains(n)),
        "{message} (expected one of {needles:?})"
    );
}

#[tokio::test]
async fn the_new_sets_bundles_are_stored_under_their_revisions_and_storing_again_is_harmless() {
    let (registry, root, mem) = (
        Registry::new(),
        tempfile::tempdir().unwrap(),
        Mem::default(),
    );
    let set = deploy(&registry, root.path(), "\nFirst revision.\n");
    store_current(&mem, &set).await.unwrap();
    store_current(&mem, &set).await.unwrap();
    let revision = set.profile(&personal()).unwrap().revision.clone();
    let stored = mem
        .bundles
        .lock()
        .unwrap()
        .get(&(personal(), revision.clone()))
        .cloned()
        .unwrap();
    assert_eq!(
        revision_of(&stored),
        revision,
        "revision must match the hash of stored bundle bytes"
    );
    assert_eq!(stored, set.bundle(&personal()).unwrap());
}

#[tokio::test]
async fn a_refused_store_stops_before_anything_else_and_names_the_profile_and_revision() {
    let cases: [(Error, &[&str]); 3] = [
        (
            Error::Conflict,
            &["different bundle", "already exists", "conflict", "corrupt"],
        ),
        (
            Error::Unavailable,
            &["unavailable", "unreachable", "reach", "connect"],
        ),
        (
            Error::Invalid,
            &["rejected", "refused", "not accept", "invalid"],
        ),
    ];
    for (error, wanted) in cases {
        let (registry, root, mem) = (
            Registry::new(),
            tempfile::tempdir().unwrap(),
            Mem::default(),
        );
        let set = deploy(&registry, root.path(), "\nSecond revision.\n");
        *mem.put_error.lock().unwrap() = Some(error);
        let message = prepare(&mem, &registry, &set, &target(&set), &Limits::default())
            .await
            .unwrap_err();
        let revision = &set.profile(&personal()).unwrap().revision.0;
        assert!(
            message.to_ascii_lowercase().contains("profile personal") && message.contains(revision),
            "{message}"
        );
        assert_any(&message, wanted);
        assert_any(&message, &["unchanged", "not changed", "was not changed"]);
    }
}

#[tokio::test]
async fn an_older_revision_the_work_pins_is_restored_and_a_missing_one_stops_the_start_with_guidance()
 {
    let (registry, root, mem) = (
        Registry::new(),
        tempfile::tempdir().unwrap(),
        Mem::default(),
    );
    let old = deploy(&registry, root.path(), "\nOld revision.\n");
    store_current(&mem, &old).await.unwrap();
    let old_revision = old.profile(&personal()).unwrap().revision.clone();
    let fresh = Registry::new();
    let new = deploy(&fresh, root.path(), "\nNew revision.\n");
    let new_revision = new.profile(&personal()).unwrap().revision.clone();
    assert_ne!(old_revision, new_revision);
    mem.pinned.lock().unwrap().extend([
        (personal(), old_revision.clone()),
        (personal(), new_revision.clone()),
        (ProfileId("gone".into()), Revision("f".repeat(64))),
    ]);
    let restored = prepare(&mem, &fresh, &new, &target(&new), &Limits::default())
        .await
        .unwrap();
    assert_eq!(
        restored.restored(),
        1,
        "only the historical revision of an active profile should be restored"
    );
    assert!(fresh.lookup(&personal(), &old_revision).is_some());
    assert_eq!(
        fresh.active().get(&personal()).unwrap().revision,
        new_revision
    );
    assert!(
        mem.puts
            .lock()
            .unwrap()
            .contains(&(personal(), new_revision.clone()))
    );
    drop(restored);

    let lost = Revision("a".repeat(64));
    mem.pinned
        .lock()
        .unwrap()
        .insert((personal(), lost.clone()));
    let fresh = Registry::new();
    let again = deploy(&fresh, root.path(), "\nNew revision.\n");
    let message = prepare(&mem, &fresh, &again, &target(&again), &Limits::default())
        .await
        .unwrap_err();
    assert!(
        message.contains(&format!(
            "profile personal revision {}: stored bundle is missing",
            "a".repeat(64)
        )),
        "{message}"
    );
    assert_any(&message, &["unchanged", "not changed", "were not changed"]);
    assert!(message.contains("bot profile-bundle put"), "{message}");
    assert!(
        fresh.lookup(&personal(), &old_revision).is_none(),
        "must not adopt any revision when restore fails"
    );
}

#[tokio::test]
async fn a_reference_that_appears_after_the_preparation_is_detected_and_a_refresh_restores_it() {
    let (registry, root, mem) = (
        Registry::new(),
        tempfile::tempdir().unwrap(),
        Mem::default(),
    );
    let old = deploy(&registry, root.path(), "\nOld revision.\n");
    store_current(&mem, &old).await.unwrap();
    let old_revision = old.profile(&personal()).unwrap().revision.clone();
    let fresh = Registry::new();
    let new = deploy(&fresh, root.path(), "\nNew revision.\n");
    let goal = target(&new);
    mem.bundles.lock().unwrap().clear();
    let mut prepared = prepare(&mem, &fresh, &new, &goal, &Limits::default())
        .await
        .unwrap();
    assert_eq!(prepared.restored(), 0);
    assert!(covers(&mem, &new, &goal, &prepared).await.unwrap());
    // Simulate an old process committing a run pinned to `old_revision` before publication.
    mem.pinned
        .lock()
        .unwrap()
        .insert((personal(), old_revision.clone()));
    assert!(!covers(&mem, &new, &goal, &prepared).await.unwrap());
    let before = prepared.covered().clone();
    let message = refresh(&mem, &fresh, &new, &goal, &Limits::default(), &mut prepared)
        .await
        .unwrap_err();
    assert!(
        message.contains(&format!(
            "profile personal revision {}: stored bundle is missing",
            old_revision.0
        )),
        "{message}"
    );
    assert_eq!(prepared.covered(), &before);
    mem.bundles.lock().unwrap().insert(
        (personal(), old_revision.clone()),
        old.bundle(&personal()).unwrap().to_vec(),
    );
    refresh(&mem, &fresh, &new, &goal, &Limits::default(), &mut prepared)
        .await
        .unwrap();
    assert!(
        prepared
            .covered()
            .contains(&(personal(), old_revision.clone()))
    );
    assert!(fresh.lookup(&personal(), &old_revision).is_some());
    assert!(covers(&mem, &new, &goal, &prepared).await.unwrap());
}

#[tokio::test]
async fn a_profile_that_left_the_set_never_blocks_the_start_even_with_work_and_no_bundle() {
    let (registry, root, mem) = (
        Registry::new(),
        tempfile::tempdir().unwrap(),
        Mem::default(),
    );
    let set = deploy(&registry, root.path(), "\nCurrent revision.\n");
    mem.pinned
        .lock()
        .unwrap()
        .insert((ProfileId("removed".into()), Revision("b".repeat(64))));
    let prepared = prepare(&mem, &registry, &set, &target(&set), &Limits::default())
        .await
        .unwrap();
    assert_eq!(prepared.restored(), 0);
    assert!(covers(&mem, &set, &target(&set), &prepared).await.unwrap());
    assert!(
        registry
            .lookup(&ProfileId("removed".into()), &Revision("b".repeat(64)))
            .is_none(),
        "removed profile must not be adopted"
    );
}

#[tokio::test]
async fn the_cli_also_ignores_references_of_profiles_outside_the_set_if_the_store_returned_them() {
    let (registry, root, mem) = (
        Registry::new(),
        tempfile::tempdir().unwrap(),
        Mem::default(),
    );
    let set = deploy(&registry, root.path(), "\nCurrent revision.\n");
    *mem.unfiltered.lock().unwrap() = true;
    mem.pinned
        .lock()
        .unwrap()
        .insert((ProfileId("removed".into()), Revision("c".repeat(64))));
    let prepared = prepare(&mem, &registry, &set, &target(&set), &Limits::default())
        .await
        .expect("missing bundle for a profile outside the set must not block startup");
    assert_eq!(prepared.restored(), 0);
    assert!(covers(&mem, &set, &target(&set), &prepared).await.unwrap());
}
