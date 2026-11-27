//! Tests for `Registry::restore` and `read_bundle`: atomic old-revision restoration into weak lookup cache.

use crate::support::{AGENT_MD, Files, Temp, base, edit, host, raw_bundle, write_profile};
use bot_core::{
    id::{ProfileId, Revision},
    port::{Error, Reply},
    profile::{ProfileBundles, ProfileCatalog},
};
use bot_profile::{Limits, ProblemKind, Registry, load, read_bundle, revision_of};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

type Key = (ProfileId, Revision);

#[derive(Default)]
struct Mem {
    bundles: Mutex<BTreeMap<Key, Vec<u8>>>,
    broken: Mutex<BTreeSet<Key>>,
    reads: Mutex<usize>,
}

impl ProfileBundles for Mem {
    fn put_bundle(&self, profile: ProfileId, revision: Revision, bundle: Vec<u8>) -> Reply<'_, ()> {
        self.bundles
            .lock()
            .unwrap()
            .insert((profile, revision), bundle);
        Box::pin(async { Ok(()) })
    }

    fn bundle(&self, profile: &ProfileId, revision: &Revision) -> Reply<'_, Option<Vec<u8>>> {
        *self.reads.lock().unwrap() += 1;
        let key = (profile.clone(), revision.clone());
        let answer = if self.broken.lock().unwrap().contains(&key) {
            Err(Error::Unavailable)
        } else {
            Ok(self.bundles.lock().unwrap().get(&key).cloned())
        };
        Box::pin(async move { answer })
    }

    fn created_ms(&self, _: &ProfileId, _: &Revision) -> Reply<'_, Option<i64>> {
        Box::pin(async { Ok(None) })
    }

    fn pinned_revisions<'a>(
        &'a self,
        _target: &'a BTreeMap<ProfileId, Revision>,
    ) -> Reply<'a, BTreeSet<Key>> {
        Box::pin(async { Ok(BTreeSet::new()) })
    }
}

fn personal() -> ProfileId {
    ProfileId("personal".into())
}

fn stored(mem: &Mem, files: &Files) -> Key {
    let bytes = raw_bundle(files);
    let key = (personal(), revision_of(&bytes));
    mem.bundles.lock().unwrap().insert(key.clone(), bytes);
    key
}

fn deploy(registry: &Registry, root: &Temp, files: &Files) -> Revision {
    write_profile(root.path(), "personal", files);
    let set = registry
        .reload(root.path(), &host(), &Limits::default())
        .unwrap_or_else(|report| panic!("{report}"));
    set.profile(&personal()).unwrap().revision.clone()
}

fn instructions(registry: &Registry, revision: &Revision) -> Option<String> {
    let snapshot = registry.lookup(&personal(), revision)?;
    Some(
        snapshot
            .agents
            .values()
            .next()
            .unwrap()
            .instructions
            .clone(),
    )
}

#[tokio::test]
async fn an_old_revision_is_restored_from_its_bundle_and_seen_only_through_lookup_while_held() {
    let (registry, root, mem) = (Registry::new(), Temp::new("restore"), Mem::default());
    let v1 = base();
    let v2 = edit(
        base(),
        AGENT_MD,
        "Check today's schedule",
        "Check tomorrow's schedule",
    );
    stored(&mem, &v1);
    let r1 = deploy(&registry, &root, &v1);
    let r2 = deploy(&registry, &root, &v2);
    assert_ne!(r1, r2);
    let fresh = Registry::new();
    deploy(&fresh, &root, &v2);
    assert!(fresh.lookup(&personal(), &r1).is_none());
    let wanted = BTreeSet::from([(personal(), r1.clone()), (personal(), r2.clone())]);
    let restored = fresh
        .restore(&mem, &wanted, &Limits::default())
        .await
        .unwrap();
    assert_eq!(restored.len(), 1, "active revision does not need restore");
    assert_eq!(
        restored
            .revisions()
            .map(|(p, r)| (p.clone(), r.clone()))
            .collect::<Vec<_>>(),
        [(personal(), r1.clone())]
    );
    assert!(
        instructions(&fresh, &r1)
            .unwrap()
            .contains("Check today's schedule")
    );
    assert!(
        instructions(&fresh, &r2)
            .unwrap()
            .contains("Check tomorrow's schedule")
    );
    assert_eq!(fresh.set().profile(&personal()).unwrap().revision, r2);
    assert_eq!(fresh.active().len(), 1);
    assert_eq!(fresh.current(&personal()).unwrap().revision, r2);
    // Restored snapshots are held by weak references in Registry.
    drop(restored);
    assert!(fresh.lookup(&personal(), &r1).is_none());
}

#[tokio::test]
async fn revisions_the_registry_already_serves_are_not_read_from_the_store() {
    let (registry, root, mem) = (Registry::new(), Temp::new("served"), Mem::default());
    let current = deploy(&registry, &root, &base());
    let restored = registry
        .restore(
            &mem,
            &BTreeSet::from([(personal(), current)]),
            &Limits::default(),
        )
        .await
        .unwrap();
    assert!(restored.is_empty());
    assert_eq!(*mem.reads.lock().unwrap(), 0);
    let old = stored(&mem, &edit(base(), AGENT_MD, "today's", "yesterday's"));
    let wanted = BTreeSet::from([old]);
    let first = registry
        .restore(&mem, &wanted, &Limits::default())
        .await
        .unwrap();
    assert_eq!((first.len(), *mem.reads.lock().unwrap()), (1, 1));
    let again = registry
        .restore(&mem, &wanted, &Limits::default())
        .await
        .unwrap();
    assert_eq!((again.len(), *mem.reads.lock().unwrap()), (0, 1));
}

#[tokio::test]
async fn every_problem_is_reported_with_profile_and_revision_and_nothing_is_adopted() {
    let (registry, mem) = (Registry::new(), Mem::default());
    let good = stored(&mem, &edit(base(), AGENT_MD, "today's", "two days ago"));
    let missing = (personal(), Revision("a".repeat(64)));
    let original = raw_bundle(&edit(base(), AGENT_MD, "today's", "three days ago"));
    let corrupt = (personal(), revision_of(&original));
    let mut flipped = original.clone();
    *flipped.last_mut().unwrap() ^= 1;
    mem.bundles.lock().unwrap().insert(corrupt.clone(), flipped);
    let other = edit(base(), "profile.yaml", "id: personal", "id: second");
    let other_bytes = raw_bundle(&other);
    let misfiled = (personal(), revision_of(&other_bytes));
    mem.bundles
        .lock()
        .unwrap()
        .insert(misfiled.clone(), other_bytes);
    let down = stored(&mem, &edit(base(), AGENT_MD, "today's", "four days ago"));
    mem.broken.lock().unwrap().insert(down.clone());

    let wanted = BTreeSet::from([
        good.clone(),
        missing.clone(),
        corrupt.clone(),
        misfiled.clone(),
        down.clone(),
    ]);
    let failure = registry
        .restore(&mem, &wanted, &Limits::default())
        .await
        .expect_err("any missing or unreadable revision must fail restore");
    let kinds: BTreeMap<Revision, &ProblemKind> = failure
        .problems
        .iter()
        .map(|p| (p.revision.clone(), &p.kind))
        .collect();
    assert_eq!(failure.problems.len(), 4, "{failure}");
    assert!(matches!(kinds[&missing.1], ProblemKind::Missing));
    assert!(
        matches!(kinds[&corrupt.1], ProblemKind::Unreadable(issues) if issues.iter().any(|i| i.code == "revision-mismatch"))
    );
    assert!(matches!(kinds[&misfiled.1], ProblemKind::Unreadable(_)));
    assert!(matches!(
        kinds[&down.1],
        ProblemKind::Store(Error::Unavailable)
    ));
    let text = failure.to_string();
    for (profile, revision) in [&missing, &corrupt, &misfiled, &down] {
        assert!(
            text.lines()
                .any(|line| line.contains(&profile.0) && line.contains(&revision.0)),
            "{text}"
        );
    }
    assert!(registry.lookup(&good.0, &good.1).is_none());
}

#[test]
fn a_profile_folder_gives_the_same_bundle_the_loader_would_store_without_looking_at_the_host() {
    let root = Temp::new("read-bundle");
    write_profile(root.path(), "personal", &base());
    let dir = root.path().join("personal");
    let (id, revision, bytes) = read_bundle(&dir, &Limits::default()).unwrap();
    let set = load(root.path(), &host(), &Limits::default()).unwrap();
    assert_eq!(id, personal());
    assert_eq!(revision, set.profile(&personal()).unwrap().revision);
    assert_eq!(
        bytes,
        set.bundle(&personal()).unwrap(),
        "bundle bytes must match loader output"
    );
    assert_eq!(revision_of(&bytes), revision);
    // `read_bundle` ignores host catalog checks so retired model aliases can still be bundled.
    let retired = Temp::new("read-bundle-retired");
    write_profile(
        retired.path(),
        "personal",
        &edit(
            base(),
            "profile.yaml",
            "model: assistant",
            "model: retired-alias",
        ),
    );
    assert!(load(retired.path(), &host(), &Limits::default()).is_err());
    let (_, old_revision, _) =
        read_bundle(&retired.path().join("personal"), &Limits::default()).unwrap();
    assert_ne!(old_revision, revision);
    let misnamed = Temp::new("read-bundle-misnamed");
    write_profile(misnamed.path(), "other", &base());
    assert!(read_bundle(&misnamed.path().join("other"), &Limits::default()).is_err());
    assert!(read_bundle(&root.path().join("nope"), &Limits::default()).is_err());
}
