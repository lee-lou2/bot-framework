use super::*;
use std::os::unix::fs::symlink;

mod recovery;

fn uid() -> u32 {
    bot_gateway::cli::socket::current_uid().unwrap()
}

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

// Stage and rename over the target so the OS does not immediately reuse the old inode number.
fn replace(path: &Path, text: &str) {
    let staged = path.with_file_name(format!("replacement-{}", std::process::id()));
    std::fs::write(&staged, text).unwrap();
    std::fs::rename(&staged, path).unwrap();
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

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
            "foreign backup prefix",
            good(".other.bak-0123456789abcdef", "3:4"),
        ),
        ("path traversal", good("../evil", "3:4")),
        ("short nonce", good(".token.bak-abc", "3:4")),
        ("non-hex nonce", good(".token.bak-zzzzzzzzzzzzzzzz", "3:4")),
        ("prefix only", good(".token.bak-", "3:4")),
        ("backup without original inode", good(bak, "none")),
        ("original inode without backup", good("none", "3:4")),
        ("bad original inode", good(bak, "x:y")),
        ("old journal magic", good(bak, "3:4").replace("-v2", "-v1")),
        ("trailing line", format!("{}extra=1\n", good(bak, "3:4"))),
        (
            "bad published inode",
            good(bak, "3:4").replace("1:2", "x:y"),
        ),
        (
            "bad temp filename",
            good(bak, "3:4").replace(".token.tmp-0123456789abcdef", "token"),
        ),
        (
            "wrong field order",
            good(bak, "3:4").replace("backup=", "original="),
        ),
    ] {
        assert_eq!(Journal::parse(&text, "token"), None, "{what}");
    }
    assert_eq!(Journal::parse(&good(bak, "3:4"), "other"), None);
}

#[test]
fn a_published_token_file_is_private_atomic_and_removed_only_by_its_publisher() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    assert!(!path.exists(), "claim alone must not create the file");
    claim.publish("secret-1").unwrap();
    assert_eq!(
        residue(dir.path()).len(),
        1,
        "journal exists before finalize"
    );
    claim.finalize();
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(read(&path), "secret-1\n");
    assert_eq!(residue(dir.path()), Vec::<String>::new());
    drop(claim);
    assert!(!path.exists(), "publisher removes its file on drop");
    assert!(
        dir.path().join("token.lock").exists(),
        "lock file remains unlocked on disk"
    );
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
    let error = TokenFile::claim(&path, uid()).err().unwrap();
    assert!(error.contains("another process"), "{error}");
    assert_eq!(read(&path), "running\n");
    drop(first);
    assert!(!path.exists());
}

#[test]
fn a_start_that_never_finalizes_restores_what_was_there_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "original secret").unwrap();
    let original = id_of(&path);
    drop(TokenFile::claim(&path, uid()).unwrap());
    assert_eq!(read(&path), "original secret");
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("new").unwrap();
    assert_eq!(read(&path), "new\n");
    drop(claim);
    assert_eq!(read(&path), "original secret");
    assert_eq!(id_of(&path), original, "must restore original inode");
    assert_eq!(residue(dir.path()), Vec::<String>::new());
    let fresh = dir.path().join("fresh");
    let mut claim = TokenFile::claim(&fresh, uid()).unwrap();
    claim.publish("new").unwrap();
    drop(claim);
    assert!(!fresh.exists());
    assert_eq!(residue(dir.path()), Vec::<String>::new());
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
    replace(&path, "foreign file");
    drop(claim);
    assert_eq!(read(&path), "foreign file");
}

#[test]
fn a_file_that_replaced_a_never_existing_one_survives_a_failed_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("dead-token").unwrap();
    replace(&path, "operator file\n");
    drop(claim);
    assert_eq!(read(&path), "operator file\n");
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
    std::fs::write(&path, "original secret").unwrap();
    std::fs::write(locked.join("token.lock"), "").unwrap();
    chmod(&locked, 0o500);
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    let error = claim.publish("new").unwrap_err();
    assert!(error.contains("token"), "{error}");
    drop(claim);
    assert_eq!(read(&path), "original secret");
    assert_eq!(residue(&locked), Vec::<String>::new());
    chmod(&locked, 0o700);
}

#[test]
fn a_publish_refuses_a_target_that_stopped_being_a_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let target = dir.path().join("target");
    std::fs::write(&target, "link target\n").unwrap();
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    symlink(&target, &path).unwrap();
    let error = claim.publish("new").unwrap_err();
    assert!(error.contains("regular file"), "{error}");
    drop(claim);
    assert_eq!(read(&target), "link target\n");
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
    std::fs::write(&path, "original secret\n").unwrap();
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("new").unwrap();
    let backup = backup_in(dir.path());
    replace(&backup, "operator other file\n");
    claim.finalize();
    assert_eq!(
        read(&backup),
        "operator other file\n",
        "must not delete backup whose inode changed"
    );
    assert!(!journal_path(&path).exists(), "journal is removed");
    drop(claim);
    let other = dir.path().join("other");
    std::fs::write(&other, "original secret\n").unwrap();
    let mut claim = TokenFile::claim(&other, uid()).unwrap();
    claim.publish("new").unwrap();
    std::fs::remove_file(backup_in_named(dir.path(), "other")).unwrap();
    claim.finalize();
    assert_eq!(
        residue(dir.path()).len(),
        1,
        "only the foreign backup from the first path remains"
    );
}

#[test]
fn a_commit_whose_journal_cannot_be_removed_keeps_the_backup_too() {
    if !permissions_enforced() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "original secret\n").unwrap();
    let mut claim = TokenFile::claim(&path, uid()).unwrap();
    claim.publish("new").unwrap();
    chmod(dir.path(), 0o500);
    claim.finalize();
    chmod(dir.path(), 0o700);
    assert_eq!(
        residue(dir.path()).len(),
        2,
        "backup must stay when journal deletion fails"
    );
    drop(claim);
}

#[test]
fn links_foreign_files_and_shared_directories_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, "original").unwrap();
    let link = dir.path().join("link");
    symlink(&target, &link).unwrap();
    let error = TokenFile::claim(&link, uid()).err().unwrap();
    assert!(error.contains("regular file"), "{error}");
    assert_eq!(read(&target), "original", "symlink target untouched");
    let locked = dir.path().join("locked");
    symlink(&target, dir.path().join("locked.lock")).unwrap();
    assert!(TokenFile::claim(&locked, uid()).is_err());
    assert_eq!(read(&target), "original");
    let other = dir.path().join("other");
    std::fs::write(&other, "foreign").unwrap();
    assert!(TokenFile::claim(&other, uid().wrapping_add(1)).is_err());
    assert_eq!(read(&other), "foreign");
    let open = dir.path().join("open");
    std::fs::create_dir(&open).unwrap();
    chmod(&open, 0o777);
    let error = TokenFile::claim(&open.join("token"), uid()).err().unwrap();
    assert!(error.contains("writable"), "{error}");
}
