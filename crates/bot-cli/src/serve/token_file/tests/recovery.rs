//! 복구: 확정하지 못한 발행(강제 종료·기동 실패)을 되돌리는 길과 되돌릴 수 없을 때 멈추는 경우.
use super::*;

#[test]
fn a_forced_kill_before_the_commit_is_repaired_by_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    // 원래 파일이 있었다: 발행 뒤 확정 전에 죽으면 경로엔 죽은 비밀, 원래 파일은 보관본에만 있다
    std::fs::write(&path, "원래 있던 비밀\n").unwrap();
    let original = id_of(&path);
    let mut dead = TokenFile::claim(&path, uid()).unwrap();
    dead.publish("dead-token").unwrap();
    dead.abandon();
    assert_eq!(read(&path), "dead-token\n");
    assert_eq!(residue(dir.path()).len(), 2, "보관본과 복구 기록이 남았다");
    // 다음 시작이 잠금을 잡으면 복구 기록이 가리키는 보관본으로 원래 파일을 되돌리고 기록을 치운다
    let next = TokenFile::claim(&path, uid()).unwrap();
    assert_eq!(read(&path), "원래 있던 비밀\n");
    assert_eq!(id_of(&path), original);
    assert_eq!(residue(dir.path()), Vec::<String>::new());
    drop(next);
    assert_eq!(
        read(&path),
        "원래 있던 비밀\n",
        "발행하지 않았으니 그대로다"
    );

    // 원래 파일이 없었다: 기록으로 알아보고 이 발행이 둔 죽은 비밀을 지운다
    let fresh = dir.path().join("fresh");
    let mut dead = TokenFile::claim(&fresh, uid()).unwrap();
    dead.publish("dead-token").unwrap();
    dead.abandon();
    assert_eq!(read(&fresh), "dead-token\n");
    assert_eq!(residue(dir.path()).len(), 1, "복구 기록이 남았다");
    drop(TokenFile::claim(&fresh, uid()).unwrap());
    assert!(!fresh.exists());
    assert_eq!(residue(dir.path()), Vec::<String>::new());
}

#[test]
fn a_kill_after_the_commit_leaves_no_journal_and_the_next_publish_simply_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "원래 있던 비밀\n").unwrap();
    let mut dead = TokenFile::claim(&path, uid()).unwrap();
    dead.publish("committed-token").unwrap();
    dead.finalize();
    dead.abandon();
    assert_eq!(
        residue(dir.path()),
        Vec::<String>::new(),
        "확정했으니 기록이 없다"
    );
    // 판본은 이미 새것이다: 다음 시작은 파일을 되돌리지 않는다
    let mut next = TokenFile::claim(&path, uid()).unwrap();
    assert_eq!(read(&path), "committed-token\n");
    next.publish("fresh-token").unwrap();
    next.finalize();
    assert_eq!(read(&path), "fresh-token\n");
}

#[test]
fn recovery_touches_only_the_files_the_journal_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "원래 있던 비밀\n").unwrap();
    // 운영자가 따로 만든 파일들: 이름이 보관본·임시 파일·복구 기록처럼 생겼어도 이 발행이 쓴 것이 아니다
    let theirs: Vec<(PathBuf, &str)> = [
        (".token.bak-old", "운영자의 백업"),
        (".token.bak-0000000000000000", "조각 모양만 같은 백업"),
        (".token.tmp-memo", "운영자의 메모"),
        (".token.tmp-0000000000000000", "조각 모양만 같은 임시 파일"),
        (".token2.bak-0123456789abcdef", "이름이 비슷한 다른 경로"),
        (".other.journal", "다른 경로의 기록"),
        ("token.bak", "그냥 백업"),
    ]
    .into_iter()
    .map(|(name, text)| (dir.path().join(name), text))
    .collect();
    for (file, text) in &theirs {
        std::fs::write(file, text).unwrap();
    }
    let mut dead = TokenFile::claim(&path, uid()).unwrap();
    dead.publish("dead-token").unwrap();
    dead.abandon();
    let next = TokenFile::claim(&path, uid()).unwrap();
    assert_eq!(read(&path), "원래 있던 비밀\n");
    for (file, text) in &theirs {
        assert_eq!(&read(file), text, "{}는 그대로여야 한다", file.display());
    }
    drop(next);
    // 기록이 없는 보통 시작·종료도 옆의 파일을 건드리지 않는다
    let mut normal = TokenFile::claim(&path, uid()).unwrap();
    normal.publish("n").unwrap();
    normal.finalize();
    drop(normal);
    for (file, text) in &theirs {
        assert_eq!(&read(file), text, "{}", file.display());
    }
}

#[test]
fn a_crash_before_the_original_was_moved_leaves_the_original_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "원래 있던 비밀\n").unwrap();
    let original = id_of(&path);
    // 복구 기록과 (비밀을 쓰다 만) 임시 파일은 있지만 원래 파일은 아직 옮기기 전이다(보관본이 없다)
    let temp = dir.path().join(".token.tmp-0123456789abcdef");
    std::fs::write(&temp, "쓰다 만 새 비밀\n").unwrap();
    let journal = Journal {
        temp: ".token.tmp-0123456789abcdef".into(),
        original: Some(Original {
            backup: ".token.bak-fedcba9876543210".into(),
            id: original,
        }),
        published: id_of(&temp),
    };
    std::fs::write(journal_path(&path), journal.render()).unwrap();
    drop(TokenFile::claim(&path, uid()).unwrap());
    assert_eq!(read(&path), "원래 있던 비밀\n");
    assert!(!temp.exists());
    assert_eq!(residue(dir.path()), Vec::<String>::new());
}

/// 확정하지 못한 발행이 끝나는 두 길
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// 확정 전에 시작이 실패했다(이 프로세스가 직접 되돌린다)
    FailedStart,
    /// 확정 전에 강제 종료됐다(다음 시작이 되돌린다)
    ForcedKill,
}

/// 운영자가 그 사이에 한 일
#[derive(Clone, Copy, Debug)]
enum Meddling {
    Nothing,
    /// 대상 파일을 다른 파일로 교체했다
    ReplacedTarget,
    /// 보관본을 지웠다
    DeletedBackup,
    /// 보관본을 같은 이름의 다른 파일로 바꿨다
    SwappedBackup,
}

/// 원래 파일이 있는 경로에서 발행해 확정하지 못한 채 `ending`으로 끝낸 뒤, `meddling`이 있은 상태에서 다음 시작을 시도한다
fn restart_after(dir: &Path, ending: Ending, meddling: Meddling) -> Result<(), String> {
    let path = dir.join("token");
    std::fs::write(&path, "원래 있던 비밀\n").unwrap();
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("dead-token").unwrap();
    let meddle = || match meddling {
        Meddling::Nothing => {}
        Meddling::ReplacedTarget => {
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, "운영자가 바꾼 파일\n").unwrap();
        }
        Meddling::DeletedBackup => std::fs::remove_file(backup_in(dir)).unwrap(),
        Meddling::SwappedBackup => {
            let backup = backup_in(dir);
            std::fs::remove_file(&backup).unwrap();
            std::fs::write(backup, "운영자의 다른 파일\n").unwrap();
        }
    };
    match ending {
        Ending::FailedStart => {
            meddle();
            drop(claim); // 복구는 여기서 한다(실패하면 stderr로만 알린다)
        }
        Ending::ForcedKill => {
            claim.abandon();
            meddle();
        }
    }
    TokenFile::claim(&path, uid()).map(drop)
}

#[test]
fn a_failed_start_and_the_next_start_recover_the_same_way() {
    for ending in [Ending::FailedStart, Ending::ForcedKill] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        restart_after(dir.path(), ending, Meddling::Nothing).unwrap();
        assert_eq!(read(&path), "원래 있던 비밀\n", "{ending:?}");
        assert_eq!(residue(dir.path()), Vec::<String>::new(), "{ending:?}");
    }
}

#[test]
fn a_target_replaced_before_the_commit_keeps_the_backup_and_the_journal() {
    for ending in [Ending::FailedStart, Ending::ForcedKill] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let error = restart_after(dir.path(), ending, Meddling::ReplacedTarget).unwrap_err();
        assert!(
            error.contains("이 발행이 둔 것이 아니라서"),
            "{ending:?}: {error}"
        );
        assert!(
            error.contains(".token.bak-"),
            "보관본 위치를 알려 준다: {error}"
        );
        // 운영자의 파일도, 원래 파일의 보관본도, 복구 기록도 그대로다
        assert_eq!(read(&path), "운영자가 바꾼 파일\n", "{ending:?}");
        assert_eq!(
            read(&backup_in(dir.path())),
            "원래 있던 비밀\n",
            "{ending:?}"
        );
        assert_eq!(residue(dir.path()).len(), 2, "{ending:?}");
        // 몇 번을 다시 시작해도 추측하지 않고 같은 상태로 멈춘다
        assert!(TokenFile::claim(&path, uid()).is_err());
        assert_eq!(residue(dir.path()).len(), 2);
        // 운영자가 보관본을 직접 되돌리고 기록을 지우면 다시 시작할 수 있다
        let backup = backup_in(dir.path());
        std::fs::rename(&backup, &path).unwrap();
        std::fs::remove_file(journal_path(&path)).unwrap();
        drop(TokenFile::claim(&path, uid()).unwrap());
        assert_eq!(read(&path), "원래 있던 비밀\n");
    }
}

#[test]
fn a_target_turned_into_a_link_is_not_followed_or_replaced_either() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let target = dir.path().join("target");
    std::fs::write(&target, "링크 대상\n").unwrap();
    std::fs::write(&path, "원래 있던 비밀\n").unwrap();
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("dead-token").unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(&target, &path).unwrap();
    drop(claim);
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(read(&target), "링크 대상\n");
    assert_eq!(read(&backup_in(dir.path())), "원래 있던 비밀\n");
    assert_eq!(residue(dir.path()).len(), 2);
    assert!(TokenFile::claim(&path, uid()).is_err());
}

#[test]
fn a_missing_or_different_backup_with_an_unconfirmed_file_stops_the_start() {
    for ending in [Ending::FailedStart, Ending::ForcedKill] {
        // 보관본이 없다: 원래 파일이 돌아왔다고 확인할 수 없고, 경로에는 확정되지 않은 발행 파일이 있다
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let error = restart_after(dir.path(), ending, Meddling::DeletedBackup).unwrap_err();
        assert!(error.contains("확인할 수 없어요"), "{ending:?}: {error}");
        assert_eq!(read(&path), "dead-token\n", "{ending:?}: 지우지 않는다");
        let left = residue(dir.path());
        assert_eq!(left.len(), 1, "복구 기록은 남는다: {left:?}");
        assert!(left[0].ends_with(".journal"));
        assert!(
            TokenFile::claim(&path, uid()).is_err(),
            "다시 시작해도 같다"
        );
        assert_eq!(read(&path), "dead-token\n");
        assert_eq!(residue(dir.path()), left);

        // 보관본이 같은 이름의 다른 파일이다: 원본으로 믿고 되돌리지 않는다
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let error = restart_after(dir.path(), ending, Meddling::SwappedBackup).unwrap_err();
        assert!(error.contains("확인할 수 없어요"), "{ending:?}: {error}");
        assert_eq!(read(&path), "dead-token\n");
        assert_eq!(read(&backup_in(dir.path())), "운영자의 다른 파일\n");
        assert_eq!(residue(dir.path()).len(), 2);
    }
}

#[test]
fn a_lost_original_is_never_reported_as_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "원래 있던 비밀\n").unwrap();
    let mut dead = TokenFile::claim(&path, uid()).unwrap();
    dead.publish("dead-token").unwrap();
    dead.abandon();
    // 경로도 보관본도 없다
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(backup_in(dir.path())).unwrap();
    let error = TokenFile::claim(&path, uid()).err().unwrap();
    assert!(error.contains("확인할 수 없어요"), "{error}");
    assert_eq!(residue(dir.path()).len(), 1, "복구 기록은 남는다");
}

#[test]
fn a_recovery_that_cannot_be_trusted_or_done_stops_the_start_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let crash = |text: &str| {
        std::fs::write(&path, "원래 있던 비밀\n").unwrap();
        let mut dead = TokenFile::claim(&path, uid()).unwrap();
        dead.publish(text).unwrap();
        dead.abandon();
    };
    // 기록이 올바르지 않다
    crash("dead-1");
    std::fs::write(journal_path(&path), "엉터리").unwrap();
    let error = TokenFile::claim(&path, uid()).err().unwrap();
    assert!(error.contains("복구 기록이 올바르지 않아요"), "{error}");
    assert_eq!(read(&path), "dead-1\n", "추측해서 바꾸지 않는다");
    std::fs::remove_file(journal_path(&path)).unwrap();
    for stray in residue(dir.path()) {
        std::fs::remove_file(dir.path().join(stray)).unwrap();
    }

    // 복구 기록이 링크다
    crash("dead-2");
    let journal = journal_path(&path);
    let real = dir.path().join("real-journal");
    std::fs::rename(&journal, &real).unwrap();
    symlink(&real, &journal).unwrap();
    assert!(TokenFile::claim(&path, uid()).is_err());
    assert_eq!(read(&path), "dead-2\n");
    std::fs::remove_file(&journal).unwrap();
    std::fs::remove_file(&real).unwrap();
    for stray in residue(dir.path()) {
        std::fs::remove_file(dir.path().join(stray)).unwrap();
    }

    // 복구 I/O가 실패한다(폴더에 더 쓸 수 없다): 무시하고 시작하지 않는다
    if permissions_enforced() {
        crash("dead-3");
        chmod(dir.path(), 0o500);
        let error = TokenFile::claim(&path, uid()).err().unwrap();
        chmod(dir.path(), 0o700);
        assert!(error.contains("denied"), "{error}");
        assert_eq!(read(&path), "dead-3\n", "복구하지 못했으면 그대로다");
        assert_eq!(residue(dir.path()).len(), 2);
        // 조건이 풀리면 같은 기록으로 복구된다
        drop(TokenFile::claim(&path, uid()).unwrap());
        assert_eq!(read(&path), "원래 있던 비밀\n");
        assert_eq!(residue(dir.path()), Vec::<String>::new());
    }
}
