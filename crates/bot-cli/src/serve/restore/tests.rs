//! 복원 준비의 시험(가짜 저장소): 새 집합의 묶음을 먼저 보관하고, 고정 판본 중 활성 집합에 든 프로필의 옛 판본만 복원하며,
//! 하나라도 못 되살리면 판본·프로필·이유를 알리고 아무것도 등록하지 않는다.

use super::*;
use bot_core::{
    port::Reply,
    profile::{ProfileBundles, ProfileCatalog},
};
use bot_profile::{HostCatalog, revision_of};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Default)]
struct Mem {
    bundles: Mutex<BTreeMap<(ProfileId, Revision), Vec<u8>>>,
    pinned: Mutex<BTreeSet<(ProfileId, Revision)>>,
    /// 켜면 `put_bundle`이 이 오류로 답한다
    put_error: Mutex<Option<Error>>,
    /// 보관한 순서(키만)
    puts: Mutex<Vec<(ProfileId, Revision)>>,
    /// 켜면 `pinned_revisions`가 `target`으로 거르지 않고 모든 참조를 돌려준다(저장소가 걸러 주지 않아도 CLI가 한 번 더 막는지 본다)
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
        // 저장소처럼: target의 ID들의 참조 ∪ target의 현재 쌍
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

/// 예시 프로필을 임시 폴더에 복사해(`edit`로 지침 끝에 글을 덧붙여) 읽은 집합과 그 레지스트리.
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

use std::sync::Arc;

/// 이번에 게시·합류할 집합의 프로필 → 판본(`Publication::profiles`).
fn target(set: &ProfileSet) -> BTreeMap<ProfileId, Revision> {
    set.active()
        .iter()
        .map(|(id, snapshot)| (id.clone(), snapshot.revision.clone()))
        .collect()
}

fn personal() -> ProfileId {
    ProfileId("personal".into())
}

#[tokio::test]
async fn the_new_sets_bundles_are_stored_under_their_revisions_and_storing_again_is_harmless() {
    let (registry, root, mem) = (
        Registry::new(),
        tempfile::tempdir().unwrap(),
        Mem::default(),
    );
    let set = deploy(&registry, root.path(), "\n첫째 판.\n");
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
        "판본은 보관한 바이트의 해시다"
    );
    assert_eq!(stored, set.bundle(&personal()).unwrap());
}

#[tokio::test]
async fn a_refused_store_stops_before_anything_else_and_names_the_profile_and_revision() {
    for (error, wanted) in [
        (Error::Conflict, "다른 묶음이 이미 있어요"),
        (Error::Unavailable, "저장소에 닿지 못했어요"),
        (Error::Invalid, "저장소가 받지 않았어요"),
    ] {
        let (registry, root, mem) = (
            Registry::new(),
            tempfile::tempdir().unwrap(),
            Mem::default(),
        );
        let set = deploy(&registry, root.path(), "\n둘째 판.\n");
        *mem.put_error.lock().unwrap() = Some(error);
        let message = prepare(&mem, &registry, &set, &target(&set), &Limits::default())
            .await
            .unwrap_err();
        let revision = &set.profile(&personal()).unwrap().revision.0;
        assert!(
            message.contains("프로필 personal") && message.contains(revision),
            "{message}"
        );
        assert!(
            message.contains(wanted) && message.contains("활성 판본은 바뀌지 않았어요"),
            "{message}"
        );
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
    // 옛 판본을 배포해 보관하고(구 프로세스가 한 일), 새 판본을 배포한다
    let old = deploy(&registry, root.path(), "\n옛 판.\n");
    store_current(&mem, &old).await.unwrap();
    let old_revision = old.profile(&personal()).unwrap().revision.clone();
    let fresh = Registry::new();
    let new = deploy(&fresh, root.path(), "\n새 판.\n");
    let new_revision = new.profile(&personal()).unwrap().revision.clone();
    assert_ne!(old_revision, new_revision);
    // 진행 중 작업이 옛 판본과 새 판본, 그리고 집합에 없는 프로필의 판본을 가리킨다
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
        "옛 판본 하나만(현재 판본과 집합에 없는 프로필은 대상이 아니다)"
    );
    assert!(fresh.lookup(&personal(), &old_revision).is_some());
    assert_eq!(
        fresh.active().get(&personal()).unwrap().revision,
        new_revision
    );
    // 새 집합의 묶음은 복원보다 먼저 보관됐다
    assert!(
        mem.puts
            .lock()
            .unwrap()
            .contains(&(personal(), new_revision.clone()))
    );
    drop(restored);

    // 보관한 묶음이 없는 판본(이전 버전이 보관하지 않았다)은 시작을 멈추고 프로필 ID·판본·안내를 알린다
    let lost = Revision("a".repeat(64));
    mem.pinned
        .lock()
        .unwrap()
        .insert((personal(), lost.clone()));
    let fresh = Registry::new();
    let again = deploy(&fresh, root.path(), "\n새 판.\n");
    let message = prepare(&mem, &fresh, &again, &target(&again), &Limits::default())
        .await
        .unwrap_err();
    assert!(
        message.contains(&format!(
            "프로필 personal 판본 {}: 보관한 묶음이 없어요",
            "a".repeat(64)
        )),
        "{message}"
    );
    assert!(
        message.contains("활성 판본·실행·대기 카드·시도 횟수는 바뀌지 않았어요"),
        "{message}"
    );
    assert!(message.contains("bot profile-bundle put"), "{message}");
    assert!(
        fresh.lookup(&personal(), &old_revision).is_none(),
        "하나라도 못 되살리면 아무것도 등록하지 않는다"
    );
}

#[tokio::test]
async fn a_reference_that_appears_after_the_preparation_is_detected_and_a_refresh_restores_it() {
    let (registry, root, mem) = (
        Registry::new(),
        tempfile::tempdir().unwrap(),
        Mem::default(),
    );
    let old = deploy(&registry, root.path(), "\n옛 판.\n");
    store_current(&mem, &old).await.unwrap();
    let old_revision = old.profile(&personal()).unwrap().revision.clone();
    let fresh = Registry::new();
    let new = deploy(&fresh, root.path(), "\n새 판.\n");
    let goal = target(&new);
    // 준비 때는 옛 판본을 아무도 참조하지 않는다: 옛 번들이 없어도 준비가 끝나고 필수 목록은 새 집합의 현재 쌍뿐이다
    mem.bundles.lock().unwrap().clear();
    let mut prepared = prepare(&mem, &fresh, &new, &goal, &Limits::default())
        .await
        .unwrap();
    assert_eq!(prepared.restored(), 0);
    assert!(covers(&mem, &new, &goal, &prepared).await.unwrap());
    // 준비 뒤 게시 전에 옛 프로세스가 옛 판본의 실행을 접수했다: 준비한 목록 밖의 참조다
    mem.pinned
        .lock()
        .unwrap()
        .insert((personal(), old_revision.clone()));
    assert!(!covers(&mem, &new, &goal, &prepared).await.unwrap());
    // 옛 번들이 없으면 목록을 다시 준비하지 못하고 판본·프로필·안내를 알린다(이미 준비한 것은 그대로)
    let before = prepared.covered().clone();
    let message = refresh(&mem, &fresh, &new, &goal, &Limits::default(), &mut prepared)
        .await
        .unwrap_err();
    assert!(
        message.contains(&format!(
            "프로필 personal 판본 {}: 보관한 묶음이 없어요",
            old_revision.0
        )),
        "{message}"
    );
    assert_eq!(prepared.covered(), &before);
    // 정확한 원본을 보충하면 다시 준비돼 필수 목록을 모두 덮는다
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
    let set = deploy(&registry, root.path(), "\n현재 판.\n");
    // 집합에서 빠진 프로필(B)의 실행·dead letter·구간이 번들 없이 남아 있다
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
        "빠진 프로필은 등록하지 않는다"
    );
}

#[tokio::test]
async fn the_cli_also_ignores_references_of_profiles_outside_the_set_if_the_store_returned_them() {
    let (registry, root, mem) = (
        Registry::new(),
        tempfile::tempdir().unwrap(),
        Mem::default(),
    );
    let set = deploy(&registry, root.path(), "\n현재 판.\n");
    *mem.unfiltered.lock().unwrap() = true;
    mem.pinned
        .lock()
        .unwrap()
        .insert((ProfileId("removed".into()), Revision("c".repeat(64))));
    let prepared = prepare(&mem, &registry, &set, &target(&set), &Limits::default())
        .await
        .expect("집합 밖 프로필의 번들이 없어도 시작을 막지 않는다");
    assert_eq!(prepared.restored(), 0);
    assert!(covers(&mem, &set, &target(&set), &prepared).await.unwrap());
}
