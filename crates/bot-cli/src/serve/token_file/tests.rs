use super::*;
use std::os::unix::fs::symlink;

mod recovery;

fn uid() -> u32 {
    bot_gateway::cli::socket::current_uid().unwrap()
}

/// root는 폴더 권한을 무시하므로 읽기 전용 폴더로 만드는 실패 시험은 건너뛴다
fn permissions_enforced() -> bool {
    uid() != 0
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn id_of(path: &Path) -> Id {
    let meta = std::fs::metadata(path).unwrap();
    (meta.dev(), meta.ino())
}

/// 같은 이름에 다른 파일을 놓는다. 지우고 다시 만들면 Linux 파일시스템은 방금 지운 inode 번호를 곧바로 다시 써서 «다른 파일»이 같은 (장치, inode)로
/// 보인다 — 새 파일을 먼저 만들어 옛 파일 위로 옮겨 놓아 두 inode가 겹치지 않게 한다.
fn replace(path: &Path, text: &str) {
    let staged = path.with_file_name(format!("replacement-{}", std::process::id()));
    std::fs::write(&staged, text).unwrap();
    std::fs::rename(&staged, path).unwrap();
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// 이 폴더에 남은 임시·보관·복구 기록 파일 이름
fn residue(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp-") || n.contains(".bak-") || n.ends_with(".journal"))
        .collect();
    found.sort();
    found
}

fn backup_in(dir: &Path) -> PathBuf {
    let names: Vec<String> = residue(dir)
        .into_iter()
        .filter(|n| n.contains(".bak-"))
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    dir.join(&names[0])
}

/// 이름이 `.<name>.bak-`로 시작하는 보관본 하나
fn backup_in_named(dir: &Path, name: &str) -> PathBuf {
    let prefix = format!(".{name}.bak-");
    let found: Vec<String> = residue(dir)
        .into_iter()
        .filter(|n| n.starts_with(&prefix))
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    dir.join(&found[0])
}

#[test]
fn the_journal_round_trips_and_only_accepts_names_this_path_could_have_written() {
    let journal = Journal {
        temp: ".token.tmp-0123456789abcdef".into(),
        original: Some(Original {
            backup: ".token.bak-fedcba9876543210".into(),
            id: (16777234, 99),
        }),
        published: (16777234, 4242),
    };
    assert_eq!(Journal::parse(&journal.render(), "token"), Some(journal));
    let none = Journal {
        temp: ".token.tmp-0123456789abcdef".into(),
        original: None,
        published: (1, 2),
    };
    assert_eq!(Journal::parse(&none.render(), "token"), Some(none));
    let good = |backup: &str, original: &str| {
        format!(
            "{MAGIC}\ntemp=.token.tmp-0123456789abcdef\nbackup={backup}\noriginal={original}\npublished=1:2\n"
        )
    };
    let bak = ".token.bak-0123456789abcdef";
    assert!(Journal::parse(&good(bak, "3:4"), "token").is_some());
    assert!(Journal::parse(&good("none", "none"), "token").is_some());
    for (what, text) in [
        (
            "다른 이름의 보관본",
            good(".other.bak-0123456789abcdef", "3:4"),
        ),
        ("경로를 벗어난 이름", good("../evil", "3:4")),
        ("짧은 조각", good(".token.bak-abc", "3:4")),
        (
            "16진이 아닌 조각",
            good(".token.bak-zzzzzzzzzzzzzzzz", "3:4"),
        ),
        ("접두사만", good(".token.bak-", "3:4")),
        ("원본 inode 없는 보관본", good(bak, "none")),
        ("보관본 없는 원본 inode", good("none", "3:4")),
        ("원본 inode 오류", good(bak, "x:y")),
        ("옛 기록 머리글", good(bak, "3:4").replace("-v2", "-v1")),
        ("남는 줄", format!("{}extra=1\n", good(bak, "3:4"))),
        ("발행 inode 오류", good(bak, "3:4").replace("1:2", "x:y")),
        (
            "임시 파일 이름 오류",
            good(bak, "3:4").replace(".token.tmp-0123456789abcdef", "token"),
        ),
        (
            "줄 순서 오류",
            good(bak, "3:4").replace("backup=", "original="),
        ),
    ] {
        assert_eq!(Journal::parse(&text, "token"), None, "{what}");
    }
    // 다른 경로의 기록은 이 경로에서 받지 않는다
    assert_eq!(Journal::parse(&good(bak, "3:4"), "other"), None);
}

#[test]
fn a_published_token_file_is_private_atomic_and_removed_only_by_its_publisher() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    assert!(!path.exists(), "독점만으로는 파일을 만들지 않는다");
    claim.publish("secret-1").unwrap();
    assert_eq!(residue(dir.path()).len(), 1, "확정 전에는 복구 기록이 있다");
    claim.finalize();
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(read(&path), "secret-1\n");
    assert_eq!(residue(dir.path()), Vec::<String>::new());
    drop(claim);
    assert!(!path.exists(), "자기가 발행한 파일은 종료할 때 지운다");
    assert!(
        dir.path().join("token.lock").exists(),
        "잠금 파일은 남겨 둔다(잠금은 풀렸다)"
    );
    // 잠금이 풀렸으니 다음 시작이 같은 경로를 쓸 수 있다
    let mut again = TokenFile::claim(&path, uid()).unwrap();
    again.publish("secret-2").unwrap();
    again.finalize();
    assert_eq!(read(&path), "secret-2\n");
}

#[test]
fn another_live_process_keeps_its_path_and_its_file_when_a_second_start_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let mut first = TokenFile::claim(&path, uid()).unwrap();
    first.publish("running").unwrap();
    first.finalize();
    // 같은 경로를 쓰려는 두 번째 시작은 독점하지 못하고, 첫 번째 파일은 그대로다
    let error = TokenFile::claim(&path, uid()).err().unwrap();
    assert!(error.contains("다른 프로세스"), "{error}");
    assert_eq!(read(&path), "running\n");
    drop(first);
    assert!(!path.exists());
}

#[test]
fn a_start_that_never_finalizes_restores_what_was_there_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "원래 있던 비밀").unwrap();
    let original = id_of(&path);
    // 독점만 하고 놓으면 그대로
    drop(TokenFile::claim(&path, uid()).unwrap());
    assert_eq!(read(&path), "원래 있던 비밀");
    // 발행했다가 확정하지 못하고 놓이면(활성 판본 확정 실패) 발행을 되돌려 원래 파일(같은 inode)을 복구한다
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("new").unwrap();
    assert_eq!(read(&path), "new\n");
    drop(claim);
    assert_eq!(read(&path), "원래 있던 비밀");
    assert_eq!(id_of(&path), original, "복사본이 아니라 원래 파일이다");
    assert_eq!(residue(dir.path()), Vec::<String>::new());
    // 처음부터 파일이 없었으면 발행분을 지운다
    let fresh = dir.path().join("fresh");
    let mut claim = TokenFile::claim(&fresh, uid()).unwrap();
    claim.publish("new").unwrap();
    drop(claim);
    assert!(!fresh.exists());
    assert_eq!(residue(dir.path()), Vec::<String>::new());
    // 확정하면 복구 기록과 보관본은 버려지고 종료할 때 발행분을 지운다
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("kept").unwrap();
    claim.finalize();
    assert_eq!(residue(dir.path()), Vec::<String>::new());
    drop(claim);
    assert!(!path.exists());
}

#[test]
fn a_file_swapped_in_after_publishing_is_never_deleted_by_the_old_publisher() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("mine").unwrap();
    claim.finalize();
    replace(&path, "남이 둔 파일");
    drop(claim);
    assert_eq!(read(&path), "남이 둔 파일");
}

#[test]
fn a_file_that_replaced_a_never_existing_one_survives_a_failed_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("dead-token").unwrap();
    replace(&path, "운영자가 둔 파일\n");
    drop(claim);
    assert_eq!(read(&path), "운영자가 둔 파일\n");
    assert_eq!(residue(dir.path()), Vec::<String>::new());
}

#[test]
fn a_publish_that_fails_leaves_the_existing_file_and_no_leftovers() {
    if !permissions_enforced() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("ro");
    std::fs::create_dir(&locked).unwrap();
    let path = locked.join("token");
    std::fs::write(&path, "원래 있던 비밀").unwrap();
    // 잠금 파일은 미리 만들어 독점은 되지만 폴더에는 더 쓸 수 없다(발행이 실패한다)
    std::fs::write(locked.join("token.lock"), "").unwrap();
    chmod(&locked, 0o500);
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    let error = claim.publish("new").unwrap_err();
    assert!(error.contains("token"), "{error}");
    drop(claim);
    assert_eq!(read(&path), "원래 있던 비밀");
    assert_eq!(residue(&locked), Vec::<String>::new());
    chmod(&locked, 0o700);
}

#[test]
fn a_publish_refuses_a_target_that_stopped_being_a_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let target = dir.path().join("target");
    std::fs::write(&target, "링크 대상\n").unwrap();
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    symlink(&target, &path).unwrap(); // 독점한 뒤에 링크가 놓였다
    let error = claim.publish("new").unwrap_err();
    assert!(error.contains("링크 아님"), "{error}");
    drop(claim);
    assert_eq!(read(&target), "링크 대상\n");
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(residue(dir.path()), Vec::<String>::new());
}

#[test]
fn a_commit_discards_only_the_backup_that_is_still_the_recorded_original() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "원래 있던 비밀\n").unwrap();
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("new").unwrap();
    // 그 사이 운영자가 보관본을 같은 이름의 다른 파일로 바꿨다
    let backup = backup_in(dir.path());
    replace(&backup, "운영자의 다른 파일\n");
    claim.finalize();
    assert_eq!(
        read(&backup),
        "운영자의 다른 파일\n",
        "기록한 원본이 아니면 지우지 않는다"
    );
    assert!(!journal_path(&path).exists(), "기록은 지운다");
    drop(claim);
    // 보관본이 이미 없어도 확정은 조용히 끝난다
    let other = dir.path().join("other");
    std::fs::write(&other, "원래 있던 비밀\n").unwrap();
    let mut claim = TokenFile::claim(&other, uid()).unwrap();
    claim.publish("new").unwrap();
    std::fs::remove_file(backup_in_named(dir.path(), "other")).unwrap();
    claim.finalize();
    assert_eq!(
        residue(dir.path()).len(),
        1,
        "첫 경로의 남의 보관본만 남는다"
    );
}

#[test]
fn a_commit_whose_journal_cannot_be_removed_keeps_the_backup_too() {
    if !permissions_enforced() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "원래 있던 비밀\n").unwrap();
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("new").unwrap();
    chmod(dir.path(), 0o500);
    claim.finalize();
    chmod(dir.path(), 0o700);
    assert_eq!(
        residue(dir.path()).len(),
        2,
        "기록을 지우지 못했으니 기록이 가리키는 보관본도 남는다"
    );
    drop(claim);
}

#[test]
fn links_foreign_files_and_shared_directories_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, "원본").unwrap();
    let link = dir.path().join("link");
    symlink(&target, &link).unwrap();
    let error = TokenFile::claim(&link, uid()).err().unwrap();
    assert!(error.contains("링크 아님"), "{error}");
    assert_eq!(read(&target), "원본", "링크 대상은 건드리지 않는다");
    // 잠금 파일 자리의 링크도 거절한다
    let locked = dir.path().join("locked");
    symlink(&target, dir.path().join("locked.lock")).unwrap();
    assert!(TokenFile::claim(&locked, uid()).is_err());
    assert_eq!(read(&target), "원본");
    // 다른 사용자 소유로 보이는 파일은 바꾸지 않는다
    let other = dir.path().join("other");
    std::fs::write(&other, "남의 것").unwrap();
    assert!(TokenFile::claim(&other, uid().wrapping_add(1)).is_err());
    assert_eq!(read(&other), "남의 것");
    // 다른 사용자가 쓸 수 있는 폴더는 거절한다
    let open = dir.path().join("open");
    std::fs::create_dir(&open).unwrap();
    chmod(&open, 0o777);
    let error = TokenFile::claim(&open.join("token"), uid()).err().unwrap();
    assert!(error.contains("다른 사용자가 쓸 수 없어야"), "{error}");
}
