use super::*;
use serde_json::json;
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    time::{Duration, SystemTime},
};

mod remote;
mod state;

fn write(path: &Path, text: &str, mode: u32) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn package(root: &Path) -> ServerSpec {
    write(
        &root.join("bin/server"),
        "#!/bin/sh\necho \"server $@\"\n",
        0o755,
    );
    write(&root.join("data.txt"), "help", 0o644);
    ServerSpec {
        root: root.to_owned(),
        command: vec!["bin/server".into(), "--stdio".into()],
        state: Default::default(),
    }
}

fn specs(spec: ServerSpec) -> BTreeMap<String, ServerSpec> {
    BTreeMap::from([("agenda".to_owned(), spec)])
}

fn revision_of_spec(spec: &ServerSpec) -> Revision {
    measure(&specs(spec.clone()))
        .unwrap()
        .remove("agenda")
        .unwrap()
}

fn uid() -> u32 {
    bot_gateway::cli::socket::current_uid().unwrap()
}

#[test]
fn the_servers_section_is_an_object_of_root_and_command() {
    let ok = parse_servers(&json!({
        "agenda": {"root": "/opt/bot/servers/agenda", "command": ["bin/server", "--stdio", ""]}
    }))
    .unwrap();
    assert_eq!(
        ok["agenda"],
        ServerSpec {
            root: "/opt/bot/servers/agenda".into(),
            command: vec!["bin/server".into(), "--stdio".into(), "".into()],
            state: Default::default(),
        }
    );
    assert!(parse_servers(&json!({})).unwrap().is_empty());
    for (value, needle) in [
        (json!(["demo-agenda"]), "object"),
        (json!("demo"), "object"),
        (
            json!({"Agenda": {"root": "/a", "command": ["x"]}}),
            "servers.Agenda",
        ),
        (json!({"agenda": "/a"}), "object"),
        (json!({"agenda": {"command": ["x"]}}), "root"),
        (
            json!({"agenda": {"root": "relative/dir", "command": ["x"]}}),
            "absolute",
        ),
        (
            json!({"agenda": {"root": "/", "command": ["x"]}}),
            "absolute",
        ),
        (json!({"agenda": {"root": "/a"}}), "command"),
        (json!({"agenda": {"root": "/a", "command": []}}), "command"),
        (json!({"agenda": {"root": "/a", "command": "x"}}), "command"),
        (json!({"agenda": {"root": "/a", "command": [1]}}), "string"),
        (
            json!({"agenda": {"root": "/a", "command": ["/bin/sh"]}}),
            "command[0]",
        ),
        (
            json!({"agenda": {"root": "/a", "command": ["../x"]}}),
            "command[0]",
        ),
        (
            json!({"agenda": {"root": "/a", "command": ["a//b"]}}),
            "command[0]",
        ),
        (
            json!({"agenda": {"root": "/a", "command": [""]}}),
            "command[0]",
        ),
        (
            json!({"agenda": {"root": "/a", "command": ["x"], "revision": "abc"}}),
            "revision",
        ),
        (
            json!({"agenda": {"root": "/a", "command": ["x"], "env": {}}}),
            "unknown",
        ),
    ] {
        let message = parse_servers(&value).unwrap_err();
        assert!(message.contains(needle), "{value} -> {message}");
    }
    let many: Vec<String> = (0..=MAX_ARGS).map(|n| n.to_string()).collect();
    assert!(parse_servers(&json!({"agenda": {"root": "/a", "command": many}})).is_err());
}

#[test]
fn the_revision_comes_from_content_and_command_not_from_location_or_time() {
    let one = tempfile::tempdir().unwrap();
    let two = tempfile::tempdir().unwrap();
    let a = package(one.path());
    let b = package(two.path());
    let original = revision_of_spec(&a);
    assert_eq!(
        original,
        revision_of_spec(&b),
        "install location must not affect revision"
    );
    let later = SystemTime::now() + Duration::from_secs(3600);
    File::options()
        .write(true)
        .open(two.path().join("data.txt"))
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(original, revision_of_spec(&b));
    let mut other_args = a.clone();
    other_args.command.push("--debug".into());
    assert_ne!(original, revision_of_spec(&other_args));
    write(&two.path().join("data.txt"), "other help", 0o644);
    assert_ne!(original, revision_of_spec(&b));
    write(&two.path().join("data.txt"), "help", 0o755);
    assert_ne!(original, revision_of_spec(&b), "executable bit");
    write(&two.path().join("data.txt"), "help", 0o644);
    assert_eq!(original, revision_of_spec(&b));
    std::fs::create_dir(two.path().join("empty")).unwrap();
    assert_ne!(
        original,
        revision_of_spec(&b),
        "empty directories affect revision"
    );
}

#[test]
fn links_special_files_and_a_non_executable_entry_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("pkg");
    let spec = package(&root);
    assert!(measure(&specs(spec.clone())).is_ok());
    symlink("/etc/hosts", root.join("link")).unwrap();
    let error = measure(&specs(spec.clone())).unwrap_err();
    assert!(
        error.contains("symlinks and special files") && error.contains("link"),
        "{error}"
    );
    std::fs::remove_file(root.join("link")).unwrap();
    let alias = dir.path().join("alias");
    symlink(&root, &alias).unwrap();
    let mut linked = spec.clone();
    linked.root = alias;
    assert!(measure(&specs(linked)).is_err());
    for command in ["missing", "data.txt", "bin"] {
        let mut bad = spec.clone();
        bad.command = vec![command.into()];
        let error = measure(&specs(bad)).unwrap_err();
        assert!(error.contains("executable"), "{command}: {error}");
    }
    let mut missing = spec.clone();
    missing.root = dir.path().join("none");
    assert!(measure(&specs(missing)).is_err());
    let mut file_root = spec;
    file_root.root = root.join("data.txt");
    assert!(measure(&specs(file_root)).is_err());
}

#[test]
fn the_installed_copy_is_read_only_runs_the_copied_bytes_and_ignores_later_source_changes() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("src");
    let spec = package(&source);
    let base = dir.path().join("installs");
    let installs = install(&specs(spec.clone()), &base, uid()).unwrap();
    let installed = installs.get("agenda").unwrap().clone();
    assert_eq!(installed.revision, revision_of_spec(&spec));
    assert_eq!(installed.dir, base.join(&installed.revision.0));
    assert_eq!(installs.revisions()["agenda"], installed.revision);
    assert_eq!(installs.aliases().collect::<Vec<_>>(), ["agenda"]);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o777;
    assert_eq!(mode(&base), 0o700);
    assert_eq!(mode(&installed.dir), 0o500);
    assert_eq!(mode(&installed.dir.join("bin")), 0o500);
    assert_eq!(mode(&installed.dir.join("bin/server")), 0o500);
    assert_eq!(mode(&installed.dir.join("data.txt")), 0o400);
    assert!(std::fs::write(installed.dir.join("data.txt"), "tampered").is_err());
    assert_eq!(
        installed.argv,
        [
            installed
                .dir
                .join("bin/server")
                .to_string_lossy()
                .into_owned(),
            "--stdio".to_owned()
        ]
    );
    let ran = |argv: &[String]| {
        let output = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    assert_eq!(ran(&installed.argv), "server --stdio");
    write(
        &source.join("bin/server"),
        "#!/bin/sh\necho changed server\n",
        0o755,
    );
    assert_eq!(ran(&installed.argv), "server --stdio");
    let changed = install(&specs(spec), &base, uid()).unwrap();
    let new = changed.get("agenda").unwrap();
    assert_ne!(new.revision, installed.revision);
    assert!(installed.dir.is_dir() && new.dir.is_dir());
    assert_eq!(ran(&new.argv), "changed server");
}

fn names(base: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(base)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.ends_with(".lock"))
        .collect();
    found.sort();
    found
}

fn inode(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().ino()
}

#[test]
fn an_existing_installation_is_reused_only_when_bytes_owner_and_modes_all_match() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("src");
    let spec = package(&source);
    let base = dir.path().join("installs");
    let first = install(&specs(spec.clone()), &base, uid()).unwrap();
    let installed = first.get("agenda").unwrap().clone();
    let original_inode = inode(&installed.dir);
    let again = install(&specs(spec.clone()), &base, uid()).unwrap();
    assert_eq!(first.get("agenda"), again.get("agenda"));
    assert_eq!(inode(&installed.dir), original_inode, "reused");
    assert_eq!(
        names(&base),
        std::slice::from_ref(&installed.revision.0),
        "no staging leftovers"
    );
    drop((first, again));

    let data = installed.dir.join("data.txt");
    let unlock = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
    };
    unlock(&installed.dir, 0o700);
    unlock(&data, 0o600);
    std::fs::write(&data, "tampered").unwrap();
    let healed = install(&specs(spec.clone()), &base, uid()).unwrap();
    assert_eq!(healed.get("agenda").unwrap().revision, installed.revision);
    assert_eq!(std::fs::read_to_string(&data).unwrap(), "help");
    assert_ne!(inode(&installed.dir), original_inode, "fresh copy");
    assert_eq!(
        names(&base),
        std::slice::from_ref(&installed.revision.0),
        "stale copy removed"
    );
    drop(healed);

    for (what, path, mode) in [
        ("file", data.clone(), 0o600),
        ("dir", installed.dir.join("bin"), 0o700),
        ("root", installed.dir.clone(), 0o700),
        ("group write", data.clone(), 0o460),
    ] {
        let before = inode(&installed.dir);
        unlock(&path, mode);
        let renewed = install(&specs(spec.clone()), &base, uid()).unwrap();
        assert_eq!(
            renewed.get("agenda").unwrap().revision,
            installed.revision,
            "{what}"
        );
        assert_ne!(
            inode(&installed.dir),
            before,
            "{what}: must be a fresh copy"
        );
        let meta = |p: &Path| std::fs::metadata(p).unwrap().mode() & 0o777;
        assert_eq!(meta(&data), 0o400, "{what}: read-only mode restored");
        assert_eq!(meta(&installed.dir), 0o500, "{what}");
        assert_eq!(
            names(&base),
            std::slice::from_ref(&installed.revision.0),
            "{what}"
        );
        drop(renewed);
    }
}

#[test]
fn a_mismatched_installation_that_is_in_use_is_not_repaired_until_every_user_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("src");
    let spec = package(&source);
    let base = dir.path().join("installs");
    let running = install(&specs(spec.clone()), &base, uid()).unwrap();
    let installed = running.get("agenda").unwrap().clone();
    let data = installed.dir.join("data.txt");
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o600)).unwrap();
    let before = inode(&installed.dir);
    let error = install(&specs(spec.clone()), &base, uid()).unwrap_err();
    assert!(error.contains("another process"), "{error}");
    assert_eq!(
        inode(&installed.dir),
        before,
        "in-use directory must not be removed"
    );
    assert_eq!(std::fs::read_to_string(&data).unwrap(), "help");
    assert!(
        std::process::Command::new(&installed.argv[0])
            .output()
            .unwrap()
            .status
            .success(),
        "running server keeps executing from its installed copy"
    );
    assert_eq!(
        names(&base),
        std::slice::from_ref(&installed.revision.0),
        "rejected start leaves no staging directory"
    );
    drop(running);
    let healed = install(&specs(spec), &base, uid()).unwrap();
    assert_eq!(healed.get("agenda").unwrap().revision, installed.revision);
    assert_ne!(inode(&installed.dir), before, "repaired with a fresh copy");
    assert_eq!(
        std::fs::metadata(&data).unwrap().mode() & 0o777,
        0o400,
        "read-only mode restored"
    );
}

#[test]
fn only_owner_and_read_only_trees_pass_the_reuse_audit() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("src");
    let spec = package(&source);
    let base = dir.path().join("installs");
    let installed = install(&specs(spec), &base, uid()).unwrap();
    let dir = &installed.get("agenda").unwrap().dir;
    assert!(walk(dir, None, Some(uid())).is_ok());
    let error = walk(dir, None, Some(uid().wrapping_add(1))).unwrap_err();
    assert!(
        error.contains("owner") && error.contains("read-only"),
        "{error}"
    );
    let writable = tempfile::tempdir().unwrap();
    assert!(walk(writable.path(), None, Some(uid())).is_err());
}

#[test]
fn a_failed_install_leaves_nothing_behind_and_unsafe_bases_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("src");
    let mut spec = package(&source);
    let base = dir.path().join("installs");
    spec.command = vec!["missing".into()];
    assert!(install(&specs(spec.clone()), &base, uid()).is_err());
    assert_eq!(
        std::fs::read_dir(&base).unwrap().count(),
        0,
        "no staging directory left behind"
    );
    let open = dir.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    spec.command = vec!["bin/server".into()];
    let error = install(&specs(spec.clone()), &open, uid()).unwrap_err();
    assert!(error.contains("0700"), "{error}");
    let error = install(&specs(spec), &base, uid().wrapping_add(1)).unwrap_err();
    assert!(error.contains("owned"), "{error}");
}

#[test]
fn stale_staging_folders_are_swept_but_fresh_ones_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("installs");
    install(&BTreeMap::new(), &base, uid()).unwrap();
    for name in [".staging-old", ".staging-new"] {
        let staging = base.join(name);
        write(&staging.join("x/file"), "leftover", 0o400);
        std::fs::set_permissions(staging.join("x"), std::fs::Permissions::from_mode(0o500))
            .unwrap();
    }
    File::open(base.join(".staging-old"))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .unwrap();
    install(&BTreeMap::new(), &base, uid()).unwrap();
    assert!(
        !base.join(".staging-old").exists(),
        "stale staging directory should be swept"
    );
    assert!(
        base.join(".staging-new").exists(),
        "fresh staging directory must be preserved for concurrent starts"
    );
    remove_tree(&base.join(".staging-new"));
}

#[test]
fn the_install_base_comes_from_the_setting_or_the_home_directory() {
    let configured = Path::new("/srv/bot/installs");
    assert_eq!(
        default_base(Some(configured), Some("/home/me")).unwrap(),
        configured
    );
    assert_eq!(
        default_base(None, Some("/home/me")).unwrap(),
        Path::new("/home/me/.local/state/bot/installs")
    );
    assert!(default_base(Some(Path::new("relative")), Some("/home/me")).is_err());
    assert!(default_base(None, None).is_err());
    assert!(default_base(None, Some("relative")).is_err());
}
