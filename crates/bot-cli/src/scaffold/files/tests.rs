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
    assert!(
        error.contains("keep/old.txt") && error.to_lowercase().contains("exist"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("keep/old.txt")).unwrap(),
        "old"
    );
}

#[test]
fn undo_removes_only_empty_folders_and_never_unlinks_a_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("keep")).unwrap();
    let mut writer = Writer::new(dir.path()).unwrap();
    writer.write("keep/new.txt", "generated").unwrap();
    writer.write("fresh/deep/a.txt", "generated").unwrap();
    writer.write("empty/deep/b.txt", "generated").unwrap();
    // Simulate an editor atomic save replacing our created file by rename.
    let temp = dir.path().join("keep/new.txt.tmp");
    fs::write(&temp, "mine").unwrap();
    fs::rename(&temp, dir.path().join("keep/new.txt")).unwrap();
    fs::remove_file(dir.path().join("empty/deep/b.txt")).unwrap();
    let left = writer.undo();
    assert_eq!(
        fs::read_to_string(dir.path().join("keep/new.txt")).unwrap(),
        "mine"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("fresh/deep/a.txt")).unwrap(),
        "generated",
        "untouched files must not be unlinked"
    );
    assert!(
        !dir.path().join("empty").exists(),
        "empty directories should be removed"
    );
    assert!(
        dir.path().join("fresh/deep").is_dir(),
        "non-empty directories must remain"
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
    assert!(error.to_lowercase().contains("link"), "{error}");
    assert!(writer.require_absent(&["agents", "main"]).is_err());
    assert!(writer.file_exists(&["agents", "x"]).is_err());
    assert!(writer.child_dirs(&["agents"]).is_err());
    // Must not follow a dangling symlink either.
    assert!(writer.write("profile.yaml", "x").is_err());
    assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn the_root_is_opened_once_from_the_given_path_and_links_in_it_are_the_users_choice() {
    let code = include_str!("../files.rs");
    let code = code.split("#[cfg(test)]").next().unwrap();
    assert!(
        !code.contains("canonicalize"),
        "canonicalize-then-reopen allows a parent symlink swap TOCTOU race"
    );
    let base = tempfile::tempdir().unwrap();
    fs::create_dir(base.path().join("real")).unwrap();
    std::os::unix::fs::symlink(base.path().join("real"), base.path().join("link")).unwrap();
    let mut writer = Writer::new(&base.path().join("link")).unwrap();
    writer.write("a.txt", "x").unwrap();
    assert!(base.path().join("real/a.txt").is_file());
}

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
        "must write nothing outside the opened root fd"
    );
    assert!(base.path().join("moved/agents/main/AGENT.md").is_file());
}

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
        "staging directory must not remain"
    );
}

#[cfg(unix)]
#[test]
fn publishing_never_replaces_a_name_and_a_failed_stage_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("full")).unwrap();
    fs::write(dir.path().join("full/mine.txt"), "mine").unwrap();
    fs::create_dir(dir.path().join("empty")).unwrap();
    fs::write(dir.path().join("file"), "mine").unwrap();
    let writer = Writer::new(dir.path()).unwrap();
    for taken in ["full", "empty", "file"] {
        let mut stage = writer.stage().unwrap();
        stage.writer.write("a/b/c.txt", "x").unwrap();
        let error = stage.publish(taken).unwrap_err();
        assert!(
            error.contains(taken) && error.to_lowercase().contains("exist"),
            "{taken}: {error}"
        );
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("full/mine.txt")).unwrap(),
        "mine"
    );
    assert_eq!(
        fs::read_dir(dir.path().join("empty")).unwrap().count(),
        0,
        "must not overwrite an empty directory"
    );
    assert_eq!(fs::read_to_string(dir.path().join("file")).unwrap(), "mine");
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        3,
        "no staging directory should remain"
    );
    let expected = expected_mode();
    let mut stage = writer.stage().unwrap();
    stage.writer.write("x.txt", "x").unwrap();
    assert_eq!(
        u64::from(mode_of(&dir.path().join(stage.name()))),
        expected,
        "staging directory must start with its final mode"
    );
    stage.publish("made").unwrap();
    assert_eq!(
        u64::from(mode_of(&dir.path().join("made"))),
        expected,
        "publish must preserve mode"
    );
}

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
        .expect("must not adopt a swapped staging directory");
    let lower = error.to_lowercase();
    assert!(
        lower.contains("owner") || lower.contains("created") || lower.contains("ours"),
        "{error}"
    );
    let name = swapped.borrow().clone();
    assert_eq!(
        fs::read_to_string(dir.path().join(&name).join("precious.txt")).unwrap(),
        "keep",
        "existing content must remain untouched"
    );
}

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
    if expected_mode() != 0o700 {
        let error = result
            .as_ref()
            .err()
            .expect("must not adopt a directory with a different mode");
        let lower = error.to_lowercase();
        assert!(
            lower.contains("owner") || lower.contains("created") || lower.contains("ours"),
            "{error}"
        );
    }
    let published = match result {
        Ok(stage) => {
            stage.publish("made").unwrap();
            dir.path().join("made")
        }
        Err(_) => swapped,
    };
    assert_eq!(mode_of(&published), 0o700);
}

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
    fs::write(staged.join("a/foreign.txt"), "foreign").unwrap();
    std::os::unix::fs::symlink(outside.path(), staged.join("link")).unwrap();
    fs::create_dir_all(staged.join("other/deep")).unwrap();
    fs::write(staged.join("other/deep/x.txt"), "foreign").unwrap();
    stage.discard();
    assert!(!staged.join("a/b").exists(), "our empty dir is removed");
    assert!(!staged.join("top.txt").exists(), "our file is removed");
    assert_eq!(
        fs::read_to_string(staged.join("a/foreign.txt")).unwrap(),
        "foreign"
    );
    assert!(staged.join("link").exists(), "foreign symlink remains");
    assert_eq!(
        fs::read_to_string(staged.join("other/deep/x.txt")).unwrap(),
        "foreign",
        "must not delete recursively"
    );
    assert!(outside.path().join("keep.txt").is_file());

    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::new(dir.path()).unwrap();
    let mut stage = writer.stage().unwrap();
    stage.writer.write("a/b/c.txt", "ours").unwrap();
    stage.discard();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn only_a_folder_with_our_owner_and_the_expected_mode_looks_like_the_one_we_made() {
    let (me, expected) = (501, 0o755);
    assert!(made_by_us(FileType::Directory, me, 0o755, me, expected));
    assert!(
        made_by_us(FileType::Directory, me, 0o40755, me, expected),
        "ignores file-type bits and checks mode bits only"
    );
    assert!(
        !made_by_us(FileType::Directory, 0, 0o755, me, expected),
        "different owner"
    );
    assert!(
        !made_by_us(FileType::Directory, me, 0o700, me, expected),
        "different mode (even stricter) is not the dir we just created"
    );
    assert!(
        !made_by_us(FileType::Directory, me, 0o775, me, expected),
        "group-writable"
    );
    assert!(
        !made_by_us(FileType::Directory, me, 0o500, me, expected),
        "not writable by us"
    );
    assert!(
        !made_by_us(FileType::Symlink, me, 0o755, me, expected),
        "not a directory"
    );
    assert!(
        made_by_us(FileType::Directory, me, 0o700, me, 0o700),
        "umask 077 expects mode 0700"
    );
}
