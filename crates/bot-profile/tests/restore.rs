//! 재배포 뒤 옛 판본 복원(`Registry::restore`): 저장해 둔 묶음에서 진행 중 작업이 고정한 판본을 다시 만들어 `lookup`으로만 보이게 한다.
//! 현재 집합은 그대로이고, 복원 대상 중 하나라도 못 되살리면 아무것도 등록하지 않고 프로필 ID·판본별 이유를 모두 돌려준다.
mod support;

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
use support::{AGENT_MD, Files, Temp, base, edit, host, raw_bundle, write_profile};

type Key = (ProfileId, Revision);

/// 저장소 가짜: 묶음을 메모리에 두고 읽은 횟수를 센다. `broken`에 든 키는 저장소 오류로 답한다.
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

/// 파일 지도의 묶음을 저장소 가짜에 둔다(프로필 `personal` 아래의 판본 = 묶음의 해시).
fn stored(mem: &Mem, files: &Files) -> Key {
    let bytes = raw_bundle(files);
    let key = (personal(), revision_of(&bytes));
    mem.bundles.lock().unwrap().insert(key.clone(), bytes);
    key
}

/// 폴더에 쓰고 레지스트리를 그 판본으로 교체한다.
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
        "오늘 일정을 조회하고",
        "내일 일정을 조회하고",
    );
    stored(&mem, &v1);
    let r1 = deploy(&registry, &root, &v1);
    let r2 = deploy(&registry, &root, &v2);
    assert_ne!(r1, r2);
    // 재시작한 새 프로세스: 현재 집합(v2)만 안다
    let fresh = Registry::new();
    deploy(&fresh, &root, &v2);
    assert!(fresh.lookup(&personal(), &r1).is_none());
    let wanted = BTreeSet::from([(personal(), r1.clone()), (personal(), r2.clone())]);
    let restored = fresh
        .restore(&mem, &wanted, &Limits::default())
        .await
        .unwrap();
    assert_eq!(restored.len(), 1, "현재 집합의 판본은 복원 대상이 아니다");
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
            .contains("오늘 일정을 조회하고"),
        "옛 지침 그대로"
    );
    assert!(
        instructions(&fresh, &r2)
            .unwrap()
            .contains("내일 일정을 조회하고")
    );
    // 복원해도 활성·현재는 게시된 집합 그대로다
    assert_eq!(fresh.set().profile(&personal()).unwrap().revision, r2);
    assert_eq!(fresh.active().len(), 1);
    assert_eq!(fresh.current(&personal()).unwrap().revision, r2);
    // 강한 참조를 놓으면 더는 조회되지 않는다(등록은 약한 참조)
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
    // 이미 복원해 쥐고 있는 판본도 다시 읽지 않는다
    let old = stored(&mem, &edit(base(), AGENT_MD, "오늘", "어제"));
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
    let good = stored(&mem, &edit(base(), AGENT_MD, "오늘", "그제"));
    // 1) 보관한 묶음이 없다
    let missing = (personal(), Revision("a".repeat(64)));
    // 2) 바이트가 바뀌었다: 키는 원래 판본이지만 묶음의 해시가 다르다
    let original = raw_bundle(&edit(base(), AGENT_MD, "오늘", "엊그제"));
    let corrupt = (personal(), revision_of(&original));
    let mut flipped = original.clone();
    *flipped.last_mut().unwrap() ^= 1;
    mem.bundles.lock().unwrap().insert(corrupt.clone(), flipped);
    // 3) 다른 프로필의 묶음이 이 프로필 키 아래 있다(해시는 맞지만 profile.yaml의 id가 다르다)
    let other = edit(base(), "profile.yaml", "id: personal", "id: second");
    let other_bytes = raw_bundle(&other);
    let misfiled = (personal(), revision_of(&other_bytes));
    mem.bundles
        .lock()
        .unwrap()
        .insert(misfiled.clone(), other_bytes);
    // 4) 저장소가 읽기에 실패한다
    let down = stored(&mem, &edit(base(), AGENT_MD, "오늘", "그끄제"));
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
        .expect_err("하나라도 못 되살리면 실패다");
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
    // 사람이 읽는 글에는 프로필 ID와 판본(SHA-256 전체)이 있다
    let text = failure.to_string();
    for (profile, revision) in [&missing, &corrupt, &misfiled, &down] {
        assert!(
            text.contains(&format!("프로필 {} 판본 {}: ", profile.0, revision.0)),
            "{text}"
        );
    }
    // 되살릴 수 있었던 판본도 등록하지 않았다
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
        "로더가 보관하는 바이트와 같다"
    );
    assert_eq!(revision_of(&bytes), revision);
    // 지금 호스트가 허용하지 않는 옛 판본(모델 별칭이 철회됐다)도 읽는다 — 호스트 등록부는 보지 않는다
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
    // 폴더 이름이 profile.yaml의 id와 다르거나, 폴더가 없으면 거절한다
    let misnamed = Temp::new("read-bundle-misnamed");
    write_profile(misnamed.path(), "other", &base());
    assert!(read_bundle(&misnamed.path().join("other"), &Limits::default()).is_err());
    assert!(read_bundle(&root.path().join("nope"), &Limits::default()).is_err());
}
