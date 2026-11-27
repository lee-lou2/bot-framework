//! Tool server install spec (`servers`) validation via `bot validate` (no DB required):
//! revisions depend on content and command rather than path, and legacy arrays/symlinks/non-executables fail.
use crate::process_support::*;
use std::path::Path;

fn validate(host: &Path) -> std::process::Output {
    let profiles = examples();
    bot(
        &[
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
        ],
        &["validate"],
    )
}

fn revision_line(output: &std::process::Output) -> String {
    text(output)
        .lines()
        .find(|line| {
            let low = line.to_ascii_lowercase();
            low.starts_with("server demo-agenda:") && low.contains("rev")
        })
        .unwrap_or_else(|| panic!("missing server revision line: {}", text(output)))
        .to_owned()
}

fn host_at(dir: &Path, name: &str, root: &Path, command: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            r#"{{"version": 1, "models": {{"personal": ["assistant"]}},
                "servers": {{"demo-agenda": {{"root": "{}", "command": {command}}}}},
                "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": {}}}}}}}}}}}"#,
            root.display(),
            my_uid()
        ),
    )
    .unwrap();
    path
}

#[test]
fn the_revision_follows_content_and_command_not_location() {
    let dir = tempfile::tempdir().unwrap();
    let (one, two) = (dir.path().join("one"), dir.path().join("two"));
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    let script = "#!/bin/sh\necho server\n";
    let (root_one, root_two) = (package(&one, script), package(&two, script));
    let command = r#"["bin/server", "--stdio"]"#;
    let first = validate(&host_at(dir.path(), "one.json", &root_one, command));
    assert!(first.status.success(), "{}", text(&first));
    let second = validate(&host_at(dir.path(), "two.json", &root_two, command));
    assert!(second.status.success(), "{}", text(&second));
    assert_eq!(
        revision_line(&first),
        revision_line(&second),
        "identical content and command must yield the same revision across paths"
    );
    package(&two, "#!/bin/sh\necho other server\n");
    let changed = validate(&host_at(dir.path(), "two.json", &root_two, command));
    assert_ne!(revision_line(&first), revision_line(&changed));
    let args = validate(&host_at(
        dir.path(),
        "args.json",
        &root_one,
        r#"["bin/server", "--other"]"#,
    ));
    assert_ne!(revision_line(&first), revision_line(&args));
}

#[test]
fn old_alias_arrays_links_and_unrunnable_commands_are_refused_before_starting() {
    let dir = tempfile::tempdir().unwrap();
    let root = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let command = r#"["bin/server", "--stdio"]"#;

    let array = dir.path().join("array.json");
    std::fs::write(
        &array,
        r#"{"version": 1, "servers": ["demo-agenda"], "models": {"personal": ["assistant"]}}"#,
    )
    .unwrap();
    let refused = validate(&array);
    assert!(!refused.status.success());
    let refused_msg = text(&refused);
    let refused_low = refused_msg.to_ascii_lowercase();
    assert!(
        refused_low.contains("servers:")
            && (refused_low.contains("object") || refused_low.contains("array")),
        "{refused_msg}"
    );

    let missing = validate(&host_at(
        dir.path(),
        "missing.json",
        &dir.path().join("none"),
        command,
    ));
    assert!(!missing.status.success());
    assert!(
        text(&missing).contains("servers.demo-agenda"),
        "{}",
        text(&missing)
    );

    std::os::unix::fs::symlink("/etc/hosts", root.join("link")).unwrap();
    let linked = validate(&host_at(dir.path(), "linked.json", &root, command));
    assert!(!linked.status.success());
    let linked_msg = text(&linked);
    let linked_low = linked_msg.to_ascii_lowercase();
    assert!(
        linked_low.contains("link")
            && (linked_low.contains("symlink") || linked_low.contains("special")),
        "{linked_msg}"
    );
    std::fs::remove_file(root.join("link")).unwrap();

    std::fs::write(root.join("notes.txt"), "notes").unwrap();
    let plain = validate(&host_at(
        dir.path(),
        "plain.json",
        &root,
        r#"["notes.txt"]"#,
    ));
    assert!(!plain.status.success());
    let plain_msg = text(&plain);
    let plain_low = plain_msg.to_ascii_lowercase();
    assert!(
        plain_low.contains("command[0]") && plain_low.contains("executable"),
        "{plain_msg}"
    );
}
