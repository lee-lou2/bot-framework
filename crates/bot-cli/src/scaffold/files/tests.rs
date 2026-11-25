//! `Writer`·`Stage` 시험: 덮어쓰기 거절, 링크·경로 바꿔치기, 임시 폴더 확인과 우리가 만든 이름만 치우기.
use super::*;
use std::fs;

#[test]
fn never_overwrites_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("keep")).unwrap();
    fs::write(dir.path().join("keep/old.txt"), "old").unwrap();
    let mut writer = Writer::new(dir.path()).unwrap();
    writer.write("keep/new.txt", "n").unwrap();
    let error = writer.write("keep/old.txt", "x").unwrap_err();
    assert!(error.contains("이미 있어요"), "{error}");
    assert_eq!(
        fs::read_to_string(dir.path().join("keep/old.txt")).unwrap(),
        "old"
    );
}

/// 막음 2(되돌림 중 교체): 되돌림은 빈 폴더 `rmdir`뿐이다. 만든 파일은 — 손대지 않은 것도, 그 사이 편집기가 새 파일로 바꾼 것도 — 지우지 않고
/// 남은 경로를 돌려준다. (옛 구현은 inode를 확인한 뒤 이름으로 지워서 확인과 삭제 사이에 바뀐 파일을 지울 수 있었다.)
#[test]
fn undo_removes_only_empty_folders_and_never_unlinks_a_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("keep")).unwrap();
    let mut writer = Writer::new(dir.path()).unwrap();
    writer.write("keep/new.txt", "generated").unwrap();
    writer.write("fresh/deep/a.txt", "generated").unwrap();
    writer.write("empty/deep/b.txt", "generated").unwrap();
    // 편집기처럼: 새 파일을 먼저 만들고 이름 위로 옮긴다
    let temp = dir.path().join("keep/new.txt.tmp");
    fs::write(&temp, "mine").unwrap();
    fs::rename(&temp, dir.path().join("keep/new.txt")).unwrap();
    // 사용자가 지운 파일의 폴더는 비어 있다
    fs::remove_file(dir.path().join("empty/deep/b.txt")).unwrap();
    let left = writer.undo();
    assert_eq!(
        fs::read_to_string(dir.path().join("keep/new.txt")).unwrap(),
        "mine"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("fresh/deep/a.txt")).unwrap(),
        "generated",
        "손대지 않은 파일도 지우지 않는다"
    );
    assert!(!dir.path().join("empty").exists(), "빈 폴더는 지운다");
    assert!(
        dir.path().join("fresh/deep").is_dir(),
        "빈 폴더가 아니면 남긴다"
    );
    assert_eq!(
        left,
        ["empty/deep/b.txt", "fresh/deep/a.txt", "keep/new.txt"]
    );
}

#[test]
fn rejects_dot_segments_and_separators() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::new(dir.path()).unwrap();
    for bad in ["../x", "a/../x", "a//x", "./x", "a\\b", ""] {
        assert!(writer.write(bad, "x").is_err(), "{bad:?}");
    }
    assert!(writer.require_absent(&[".."]).is_err());
    assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn does_not_follow_links_in_existing_parents_or_at_the_end() {
    let outside = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("agents")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("target.txt"),
        dir.path().join("profile.yaml"),
    )
    .unwrap();
    let mut writer = Writer::new(dir.path()).unwrap();
    let error = writer.write("agents/main/AGENT.md", "x").unwrap_err();
    assert!(error.contains("링크"), "{error}");
    assert!(writer.require_absent(&["agents", "main"]).is_err());
    assert!(writer.file_exists(&["agents", "x"]).is_err());
    assert!(writer.child_dirs(&["agents"]).is_err());
    // 끊어진 링크도 따라가 새로 만들지 않는다
    assert!(writer.write("profile.yaml", "x").is_err());
    assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
}

/// 막음 1: 기준 폴더는 사용자가 준 경로를 한 번만 연다 — 풀어 놓고 다시 여는 길이 없고(소스에 `canonicalize`가 없다), 경로 속 링크는 사용자가
/// 고른 것이라 따라간다.
#[cfg(unix)]
#[test]
fn the_root_is_opened_once_from_the_given_path_and_links_in_it_are_the_users_choice() {
    let code = include_str!("../files.rs");
    let code = code.split("#[cfg(test)]").next().unwrap();
    assert!(
        !code.contains("canonicalize"),
        "경로를 풀어 놓고 다시 열면 그 사이 상위 폴더를 링크로 바꿔 칠 수 있다"
    );
    let base = tempfile::tempdir().unwrap();
    fs::create_dir(base.path().join("real")).unwrap();
    std::os::unix::fs::symlink(base.path().join("real"), base.path().join("link")).unwrap();
    let mut writer = Writer::new(&base.path().join("link")).unwrap();
    writer.write("a.txt", "x").unwrap();
    assert!(base.path().join("real/a.txt").is_file());
}

/// 막음 1: 기준 폴더를 연 뒤에 그 이름을 링크로 바꿔 쳐도 바깥에 쓰지 않는다(열어 둔 폴더에 쓴다).
#[cfg(unix)]
#[test]
fn a_root_swapped_for_a_link_after_opening_does_not_redirect_writes() {
    let base = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = base.path().join("root");
    fs::create_dir(&root).unwrap();
    let mut writer = Writer::new(&root).unwrap();
    fs::rename(&root, base.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(outside.path(), &root).unwrap();
    writer.write("agents/main/AGENT.md", "x").unwrap();
    assert!(
        fs::read_dir(outside.path()).unwrap().next().is_none(),
        "바꿔 친 링크 바깥에는 아무것도 쓰지 않는다"
    );
    assert!(base.path().join("moved/agents/main/AGENT.md").is_file());
}

/// 막음 1: 임시 폴더에 다 만든 뒤 상위 폴더를 링크로 바꿔 쳐도 최종 이름은 열어 둔 폴더 안에 생긴다.
#[cfg(unix)]
#[test]
fn a_staged_tree_is_published_into_the_opened_folder_even_if_its_path_is_swapped() {
    let base = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let parent = base.path().join("p");
    fs::create_dir(&parent).unwrap();
    let writer = Writer::new(&parent).unwrap();
    let mut stage = writer.stage().unwrap();
    stage
        .writer
        .write("profiles/demo/profile.yaml", "x")
        .unwrap();
    fs::rename(&parent, base.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(outside.path(), &parent).unwrap();
    stage.publish("demo").unwrap();
    assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    assert!(
        base.path()
            .join("moved/demo/profiles/demo/profile.yaml")
            .is_file()
    );
    assert_eq!(
        fs::read_dir(base.path().join("moved")).unwrap().count(),
        1,
        "임시 폴더는 남지 않는다"
    );
}

#[cfg(unix)]
#[test]
fn publishing_never_replaces_a_name_and_a_failed_stage_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // 이미 있는 이름: 내용이 있는 폴더·빈 폴더(이름 바꾸기가 그냥 덮어쓸 수 있는 것)·파일
    fs::create_dir(dir.path().join("full")).unwrap();
    fs::write(dir.path().join("full/mine.txt"), "mine").unwrap();
    fs::create_dir(dir.path().join("empty")).unwrap();
    fs::write(dir.path().join("file"), "mine").unwrap();
    let writer = Writer::new(dir.path()).unwrap();
    for taken in ["full", "empty", "file"] {
        let mut stage = writer.stage().unwrap();
        stage.writer.write("a/b/c.txt", "x").unwrap();
        let error = stage.publish(taken).unwrap_err();
        assert!(error.contains("이미 있어요"), "{taken}: {error}");
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("full/mine.txt")).unwrap(),
        "mine"
    );
    assert_eq!(
        fs::read_dir(dir.path().join("empty")).unwrap().count(),
        0,
        "빈 폴더도 덮어쓰지 않는다"
    );
    assert_eq!(fs::read_to_string(dir.path().join("file")).unwrap(), "mine");
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        3,
        "임시 폴더가 남지 않는다"
    );
    // 성공하면 폴더 권한은 만들 때부터 보통 값(0777 & !umask)이고 게시해도 그대로다
    let expected = expected_mode();
    let mut stage = writer.stage().unwrap();
    stage.writer.write("x.txt", "x").unwrap();
    assert_eq!(
        u64::from(mode_of(&dir.path().join(stage.name()))),
        expected,
        "만들 때부터 최종 권한이다"
    );
    stage.publish("made").unwrap();
    assert_eq!(
        u64::from(mode_of(&dir.path().join("made"))),
        expected,
        "게시해도 권한은 그대로다"
    );
}

/// 이 프로세스의 umask로 `mkdir(0o777)`이 만드는 권한(다른 시험이 umask를 읽는 동안과 겹치지 않게 잠그고 읽는다).
#[cfg(unix)]
fn expected_mode() -> u64 {
    let _guard = UMASK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    usual_mode()
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// 임시 폴더를 만든 뒤 열기 전의 바꿔치기를 흉내 낸다: 우리가 만든 것은 옆으로 치우고 그 이름에 `mode` 권한의 새 폴더를 둔다(`content`면 내용도 둔다).
#[cfg(unix)]
fn swap_in(dir: &Path, name: &str, mode: u32, content: bool) {
    use std::os::unix::fs::PermissionsExt;
    let staged = dir.join(name);
    fs::rename(&staged, dir.join(format!("{name}.ours"))).unwrap();
    fs::create_dir(&staged).unwrap();
    fs::set_permissions(&staged, fs::Permissions::from_mode(mode)).unwrap();
    if content {
        fs::write(staged.join("precious.txt"), "keep").unwrap();
    }
}

/// 막음 3: 임시 폴더를 만든 뒤 열기 전에 그 이름이 내용이 든 폴더로 바뀌면 그것을 잡지 않고 실패하며, 그 내용은 그대로다. 기대 권한과 같은 권한이어도
/// (소유자·권한만으로는 가려지지 않는 경우) 비어 있지 않으면 잡지 않는다.
#[cfg(unix)]
#[test]
fn a_staging_name_swapped_for_a_folder_with_content_is_never_adopted_or_emptied() {
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::new(dir.path()).unwrap();
    let swapped = std::cell::RefCell::new(String::new());
    let expected = u32::try_from(expected_mode()).unwrap();
    let error = writer
        .stage_with(|name| {
            *swapped.borrow_mut() = name.to_owned();
            swap_in(dir.path(), name, expected, true);
        })
        .err()
        .expect("바꿔치기된 폴더는 잡지 않는다");
    assert!(error.contains("우리가 방금 만든 것이 아니에요"), "{error}");
    let name = swapped.borrow().clone();
    assert_eq!(
        fs::read_to_string(dir.path().join(&name).join("precious.txt")).unwrap(),
        "keep",
        "남의 내용은 그대로다"
    );
}

/// 막음(검토자): 임시 항목이 실행 사용자 소유의 기존 빈 `0700` 폴더로 바뀌어도 그 폴더의 권한은 바뀌지 않는다 — 권한을 나중에 바꾸지 않고(옛 구현은
/// 게시할 때 `0755`로 바꿔 원래 못 열던 사람에게 열었다), 기대 권한(`0777 & !umask`)과 다른 폴더는 잡지도 않는다.
#[cfg(unix)]
#[test]
fn a_swapped_in_private_folder_of_ours_keeps_its_mode() {
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::new(dir.path()).unwrap();
    let swapped = std::cell::RefCell::new(String::new());
    let result = writer.stage_with(|name| {
        *swapped.borrow_mut() = name.to_owned();
        swap_in(dir.path(), name, 0o700, false);
    });
    let swapped = dir.path().join(swapped.borrow().as_str());
    // 보통의 umask(022 등)에서는 0700이 기대 권한과 달라 잡지 않는다(umask 077이면 같은 모양이라 잡힌다)
    if expected_mode() != 0o700 {
        let error = result
            .as_ref()
            .err()
            .expect("권한이 다른 폴더는 잡지 않는다");
        assert!(error.contains("우리가 방금 만든 것이 아니에요"), "{error}");
    }
    // 어느 쪽이든 게시해도 그 폴더의 권한은 그대로 0700이다
    let published = match result {
        Ok(stage) => {
            stage.publish("made").unwrap();
            dir.path().join("made")
        }
        Err(_) => swapped,
    };
    assert_eq!(mode_of(&published), 0o700);
}

/// 막음(검토자): 게시는 권한을 바꾸지 않는다 — 임시 폴더의 권한이 무엇이든(여기서는 게시 직전에 `0700`으로 둔다) 그대로 최종 이름이 된다.
#[cfg(unix)]
#[test]
fn publishing_never_changes_the_mode_of_what_it_moves() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let stage = Writer::new(dir.path()).unwrap().stage().unwrap();
    let staged = dir.path().join(stage.name());
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o700)).unwrap();
    stage.publish("made").unwrap();
    assert_eq!(mode_of(&dir.path().join("made")), 0o700);
}

#[cfg(unix)]
#[test]
fn discarding_a_stage_removes_only_the_names_it_created() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep.txt"), "keep").unwrap();
    let writer = Writer::new(dir.path()).unwrap();
    let mut stage = writer.stage().unwrap();
    stage.writer.write("a/b/c.txt", "ours").unwrap();
    stage.writer.write("top.txt", "ours").unwrap();
    let staged = dir.path().join(stage.name());
    // 우리가 만들지 않은 것: 폴더 안의 파일·링크·깊은 폴더
    fs::write(staged.join("a/foreign.txt"), "foreign").unwrap();
    std::os::unix::fs::symlink(outside.path(), staged.join("link")).unwrap();
    fs::create_dir_all(staged.join("other/deep")).unwrap();
    fs::write(staged.join("other/deep/x.txt"), "foreign").unwrap();
    stage.discard();
    assert!(!staged.join("a/b").exists(), "우리가 만든 빈 폴더는 지운다");
    assert!(
        !staged.join("top.txt").exists(),
        "우리가 만든 파일은 지운다"
    );
    assert_eq!(
        fs::read_to_string(staged.join("a/foreign.txt")).unwrap(),
        "foreign"
    );
    assert!(staged.join("link").exists(), "링크도 그대로다");
    assert_eq!(
        fs::read_to_string(staged.join("other/deep/x.txt")).unwrap(),
        "foreign",
        "재귀로 지우지 않는다"
    );
    assert!(outside.path().join("keep.txt").is_file());
    // 정상 경우: 우리 것만 있으면 임시 폴더까지 남지 않는다
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::new(dir.path()).unwrap();
    let mut stage = writer.stage().unwrap();
    stage.writer.write("a/b/c.txt", "ours").unwrap();
    stage.discard();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// 막음 3: 소유자·권한·종류가 하나라도 우리가 만든 모양이 아니면 잡지 않는다(시험은 한 사용자라 소유자가 다른 폴더를 만들 수 없어 판정만 확인한다).
#[test]
fn only_a_folder_with_our_owner_and_the_expected_mode_looks_like_the_one_we_made() {
    let (me, expected) = (501, 0o755);
    assert!(made_by_us(FileType::Directory, me, 0o755, me, expected));
    assert!(
        made_by_us(FileType::Directory, me, 0o40755, me, expected),
        "종류 비트가 섞여 있어도 권한만 본다"
    );
    assert!(
        !made_by_us(FileType::Directory, 0, 0o755, me, expected),
        "다른 소유자"
    );
    assert!(
        !made_by_us(FileType::Directory, me, 0o700, me, expected),
        "권한이 다른 내 폴더는(더 비공개여도) 우리가 방금 만든 것이 아니다"
    );
    assert!(
        !made_by_us(FileType::Directory, me, 0o775, me, expected),
        "그룹이 쓸 수 있다"
    );
    assert!(
        !made_by_us(FileType::Directory, me, 0o500, me, expected),
        "우리가 쓸 수 없다"
    );
    assert!(
        !made_by_us(FileType::Symlink, me, 0o755, me, expected),
        "폴더가 아니다"
    );
    assert!(
        made_by_us(FileType::Directory, me, 0o700, me, 0o700),
        "umask 077이면 기대 권한도 0700이다"
    );
}
