//! Recovery tests for uncommitted `TokenFile` publications after crashes or startup failures.

use super::*;

fn assert_any(message: &str, needles: &[&str]) {
    let lower = message.to_ascii_lowercase();
    assert!(
        needles.iter().any(|n| lower.contains(n)),
        "{message} (expected one of {needles:?})"
    );
}

#[test]
fn a_forced_kill_before_the_commit_is_repaired_by_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "original secret\n").unwrap();
    let original = id_of(&path);
    let mut dead = TokenFile::claim(&path, uid()).unwrap();
    dead.publish("dead-token").unwrap();
    dead.abandon();
    assert_eq!(read(&path), "dead-token\n");
    assert_eq!(
        residue(dir.path()).len(),
        2,
        "backup and recovery journal remain"
    );
    let next = TokenFile::claim(&path, uid()).unwrap();
    assert_eq!(read(&path), "original secret\n");
    assert_eq!(id_of(&path), original);
    assert_eq!(residue(dir.path()), Vec::<String>::new());
    drop(next);
    assert_eq!(
        read(&path),
        "original secret\n",
        "never published, so original remains"
    );

    let fresh = dir.path().join("fresh");
    let mut dead = TokenFile::claim(&fresh, uid()).unwrap();
    dead.publish("dead-token").unwrap();
    dead.abandon();
    assert_eq!(read(&fresh), "dead-token\n");
    assert_eq!(residue(dir.path()).len(), 1, "recovery journal remains");
    drop(TokenFile::claim(&fresh, uid()).unwrap());
    assert!(!fresh.exists());
    assert_eq!(residue(dir.path()), Vec::<String>::new());
}

#[test]
fn a_kill_after_the_commit_leaves_no_journal_and_the_next_publish_simply_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "original secret\n").unwrap();
    let mut dead = TokenFile::claim(&path, uid()).unwrap();
    dead.publish("committed-token").unwrap();
    dead.finalize();
    dead.abandon();
    assert_eq!(
        residue(dir.path()),
        Vec::<String>::new(),
        "finalized commit leaves no journal"
    );
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
    std::fs::write(&path, "original secret\n").unwrap();
    let theirs: Vec<(PathBuf, &str)> = [
        (".token.bak-old", "operator backup"),
        (".token.bak-0000000000000000", "same-shaped backup"),
        (".token.tmp-memo", "operator memo"),
        (".token.tmp-0000000000000000", "same-shaped temp"),
        (".token2.bak-0123456789abcdef", "similar path"),
        (".other.journal", "other journal"),
        ("token.bak", "plain backup"),
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
    assert_eq!(read(&path), "original secret\n");
    for (file, text) in &theirs {
        assert_eq!(
            &read(file),
            text,
            "{} must remain untouched",
            file.display()
        );
    }
    drop(next);
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
    std::fs::write(&path, "original secret\n").unwrap();
    let original = id_of(&path);
    let temp = dir.path().join(".token.tmp-0123456789abcdef");
    std::fs::write(&temp, "partial new secret\n").unwrap();
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
    assert_eq!(read(&path), "original secret\n");
    assert!(!temp.exists());
    assert_eq!(residue(dir.path()), Vec::<String>::new());
}

#[derive(Clone, Copy, Debug)]
enum Ending {
    FailedStart,
    ForcedKill,
}

#[derive(Clone, Copy, Debug)]
enum Meddling {
    Nothing,
    ReplacedTarget,
    DeletedBackup,
    SwappedBackup,
}

fn restart_after(dir: &Path, ending: Ending, meddling: Meddling) -> Result<(), String> {
    let path = dir.join("token");
    std::fs::write(&path, "original secret\n").unwrap();
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("dead-token").unwrap();
    let meddle = || match meddling {
        Meddling::Nothing => {}
        Meddling::ReplacedTarget => {
            replace(&path, "operator replacement\n");
        }
        Meddling::DeletedBackup => std::fs::remove_file(backup_in(dir)).unwrap(),
        Meddling::SwappedBackup => {
            let backup = backup_in(dir);
            replace(&backup, "operator other file\n");
        }
    };
    match ending {
        Ending::FailedStart => {
            meddle();
            drop(claim);
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
        assert_eq!(read(&path), "original secret\n", "{ending:?}");
        assert_eq!(residue(dir.path()), Vec::<String>::new(), "{ending:?}");
    }
}

#[test]
fn a_target_replaced_before_the_commit_keeps_the_backup_and_the_journal() {
    for ending in [Ending::FailedStart, Ending::ForcedKill] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let error = restart_after(dir.path(), ending, Meddling::ReplacedTarget).unwrap_err();
        assert_any(
            &error,
            &[
                "changed",
                "replaced",
                "not published",
                "not placed",
                "not ours",
                "did not restore",
            ],
        );
        assert!(
            error.contains(".token.bak-"),
            "error must report the backup path: {error}"
        );
        assert_eq!(read(&path), "operator replacement\n", "{ending:?}");
        assert_eq!(
            read(&backup_in(dir.path())),
            "original secret\n",
            "{ending:?}"
        );
        assert_eq!(residue(dir.path()).len(), 2, "{ending:?}");
        assert!(TokenFile::claim(&path, uid()).is_err());
        assert_eq!(residue(dir.path()).len(), 2);
        let backup = backup_in(dir.path());
        std::fs::rename(&backup, &path).unwrap();
        std::fs::remove_file(journal_path(&path)).unwrap();
        drop(TokenFile::claim(&path, uid()).unwrap());
        assert_eq!(read(&path), "original secret\n");
    }
}

#[test]
fn a_target_turned_into_a_link_is_not_followed_or_replaced_either() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let target = dir.path().join("target");
    std::fs::write(&target, "link target\n").unwrap();
    std::fs::write(&path, "original secret\n").unwrap();
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
    assert_eq!(read(&target), "link target\n");
    assert_eq!(read(&backup_in(dir.path())), "original secret\n");
    assert_eq!(residue(dir.path()).len(), 2);
    assert!(TokenFile::claim(&path, uid()).is_err());
}

#[test]
fn a_missing_or_different_backup_with_an_unconfirmed_file_stops_the_start() {
    for ending in [Ending::FailedStart, Ending::ForcedKill] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let error = restart_after(dir.path(), ending, Meddling::DeletedBackup).unwrap_err();
        assert_any(
            &error,
            &["confirm", "verify", "missing", "backup", "restore"],
        );
        assert!(error.contains(".journal"), "{ending:?}: {error}");
        assert_eq!(read(&path), "dead-token\n", "{ending:?}: must not delete");
        let left = residue(dir.path());
        assert_eq!(left.len(), 1, "recovery journal must remain: {left:?}");
        assert!(left[0].ends_with(".journal"));
        assert!(TokenFile::claim(&path, uid()).is_err());
        assert_eq!(read(&path), "dead-token\n");
        assert_eq!(residue(dir.path()), left);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let error = restart_after(dir.path(), ending, Meddling::SwappedBackup).unwrap_err();
        assert_any(
            &error,
            &["confirm", "verify", "different", "backup", "restore"],
        );
        assert!(error.contains(".journal"), "{ending:?}: {error}");
        assert_eq!(read(&path), "dead-token\n");
        assert_eq!(read(&backup_in(dir.path())), "operator other file\n");
        assert_eq!(residue(dir.path()).len(), 2);
    }
}

#[test]
fn a_lost_original_is_never_reported_as_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "original secret\n").unwrap();
    let mut dead = TokenFile::claim(&path, uid()).unwrap();
    dead.publish("dead-token").unwrap();
    dead.abandon();
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(backup_in(dir.path())).unwrap();
    let error = TokenFile::claim(&path, uid()).err().unwrap();
    assert_any(
        &error,
        &["confirm", "verify", "missing", "backup", "restore"],
    );
    assert_eq!(residue(dir.path()).len(), 1, "recovery journal must remain");
}

#[test]
fn a_recovery_that_cannot_be_trusted_or_done_stops_the_start_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let crash = |text: &str| {
        std::fs::write(&path, "original secret\n").unwrap();
        let mut dead = TokenFile::claim(&path, uid()).unwrap();
        dead.publish(text).unwrap();
        dead.abandon();
    };
    crash("dead-1");
    std::fs::write(journal_path(&path), "garbage").unwrap();
    let error = TokenFile::claim(&path, uid()).err().unwrap();
    assert_any(&error, &["invalid", "corrupt", "malformed", "journal"]);
    assert_eq!(
        read(&path),
        "dead-1\n",
        "must not alter file on bad journal"
    );
    std::fs::remove_file(journal_path(&path)).unwrap();
    for stray in residue(dir.path()) {
        std::fs::remove_file(dir.path().join(stray)).unwrap();
    }

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

    if permissions_enforced() {
        crash("dead-3");
        chmod(dir.path(), 0o500);
        let error = TokenFile::claim(&path, uid()).err().unwrap();
        chmod(dir.path(), 0o700);
        assert!(error.contains("denied"), "{error}");
        assert_eq!(
            read(&path),
            "dead-3\n",
            "failed recovery leaves file intact"
        );
        assert_eq!(residue(dir.path()).len(), 2);
        drop(TokenFile::claim(&path, uid()).unwrap());
        assert_eq!(read(&path), "original secret\n");
        assert_eq!(residue(dir.path()), Vec::<String>::new());
    }
}
