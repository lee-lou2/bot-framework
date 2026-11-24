//! 도구 서버 설치 명세(`servers`) 시험(실제 `bot validate`, DB 불필요): 판본은 패키지의 내용·명령에서 자동으로 계산되고
//! 위치와 시각에는 무관하며, 옛 별칭 배열·링크·실행할 수 없는 명령은 시작 전에 거절된다.
mod process_support;
use process_support::*;
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

/// `validate`가 찍은 서버 판본 줄(12자리)
fn revision_line(output: &std::process::Output) -> String {
    text(output)
        .lines()
        .find(|line| line.starts_with("서버 demo-agenda: 판본 "))
        .unwrap_or_else(|| panic!("서버 판본 줄이 없어요: {}", text(output)))
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
    let script = "#!/bin/sh\necho 서버\n";
    let (root_one, root_two) = (package(&one, script), package(&two, script));
    let command = r#"["bin/server", "--stdio"]"#;
    let first = validate(&host_at(dir.path(), "one.json", &root_one, command));
    assert!(first.status.success(), "{}", text(&first));
    let second = validate(&host_at(dir.path(), "two.json", &root_two, command));
    assert!(second.status.success(), "{}", text(&second));
    assert_eq!(
        revision_line(&first),
        revision_line(&second),
        "위치가 달라도 같은 판본"
    );
    // 내용이 바뀌거나 명령 인자가 바뀌면 다른 판본
    package(&two, "#!/bin/sh\necho 다른 서버\n");
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
    // 옛 별칭 배열
    let array = dir.path().join("array.json");
    std::fs::write(
        &array,
        r#"{"version": 1, "servers": ["demo-agenda"], "models": {"personal": ["assistant"]}}"#,
    )
    .unwrap();
    let refused = validate(&array);
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("옛 별칭 배열"),
        "{}",
        text(&refused)
    );
    // 없는 설치 폴더
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
    // 링크
    std::os::unix::fs::symlink("/etc/hosts", root.join("link")).unwrap();
    let linked = validate(&host_at(dir.path(), "linked.json", &root, command));
    assert!(!linked.status.success());
    assert!(
        text(&linked).contains("링크·특수 파일"),
        "{}",
        text(&linked)
    );
    std::fs::remove_file(root.join("link")).unwrap();
    // 실행 파일이 아닌 command[0]
    std::fs::write(root.join("notes.txt"), "메모").unwrap();
    let plain = validate(&host_at(
        dir.path(),
        "plain.json",
        &root,
        r#"["notes.txt"]"#,
    ));
    assert!(!plain.status.success());
    assert!(
        text(&plain).contains("실행 가능한 일반 파일"),
        "{}",
        text(&plain)
    );
}
