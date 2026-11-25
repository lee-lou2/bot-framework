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

/// `bin/server`(실행 스크립트)와 `data.txt`가 있는 패키지
fn package(root: &Path) -> ServerSpec {
    write(
        &root.join("bin/server"),
        "#!/bin/sh\necho \"서버 $@\"\n",
        0o755,
    );
    write(&root.join("data.txt"), "도움말", 0o644);
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
        (json!(["demo-agenda"]), "객체"),
        (json!("demo"), "객체"),
        (
            json!({"Agenda": {"root": "/a", "command": ["x"]}}),
            "이름 규칙",
        ),
        (json!({"agenda": "/a"}), "객체여야"),
        (json!({"agenda": {"command": ["x"]}}), "root"),
        (
            json!({"agenda": {"root": "relative/dir", "command": ["x"]}}),
            "절대 경로",
        ),
        (
            json!({"agenda": {"root": "/", "command": ["x"]}}),
            "절대 경로",
        ),
        (json!({"agenda": {"root": "/a"}}), "command"),
        (json!({"agenda": {"root": "/a", "command": []}}), "command"),
        (json!({"agenda": {"root": "/a", "command": "x"}}), "command"),
        (json!({"agenda": {"root": "/a", "command": [1]}}), "문자열"),
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
            "자동으로 계산",
        ),
        (
            json!({"agenda": {"root": "/a", "command": ["x"], "env": {}}}),
            "알 수 없는 키",
        ),
    ] {
        let message = parse_servers(&value).unwrap_err();
        assert!(message.contains(needle), "{value} → {message}");
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
    assert_eq!(original, revision_of_spec(&b), "설치 위치가 달라도 같다");
    // 수정 시각이 달라도 같다
    let later = SystemTime::now() + Duration::from_secs(3600);
    File::options()
        .write(true)
        .open(two.path().join("data.txt"))
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(original, revision_of_spec(&b));
    // 명령 인자·파일 내용·실행 비트·빈 폴더는 판본을 바꾼다
    let mut other_args = a.clone();
    other_args.command.push("--debug".into());
    assert_ne!(original, revision_of_spec(&other_args));
    write(&two.path().join("data.txt"), "다른 도움말", 0o644);
    assert_ne!(original, revision_of_spec(&b));
    write(&two.path().join("data.txt"), "도움말", 0o755);
    assert_ne!(original, revision_of_spec(&b), "실행 비트");
    write(&two.path().join("data.txt"), "도움말", 0o644);
    assert_eq!(original, revision_of_spec(&b));
    std::fs::create_dir(two.path().join("empty")).unwrap();
    assert_ne!(original, revision_of_spec(&b), "빈 폴더도 판본에 든다");
}

#[test]
fn links_special_files_and_a_non_executable_entry_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("pkg");
    let spec = package(&root);
    assert!(measure(&specs(spec.clone())).is_ok());
    // 링크는 거절한다(밖의 파일을 끌어오지 못한다)
    symlink("/etc/hosts", root.join("link")).unwrap();
    let error = measure(&specs(spec.clone())).unwrap_err();
    assert!(
        error.contains("링크·특수 파일") && error.contains("link"),
        "{error}"
    );
    std::fs::remove_file(root.join("link")).unwrap();
    // 루트 자체가 링크여도 거절한다
    let alias = dir.path().join("alias");
    symlink(&root, &alias).unwrap();
    let mut linked = spec.clone();
    linked.root = alias;
    assert!(measure(&specs(linked)).is_err());
    // command[0]이 없거나 실행 파일이 아니거나 폴더면 거절한다
    for command in ["missing", "data.txt", "bin"] {
        let mut bad = spec.clone();
        bad.command = vec![command.into()];
        let error = measure(&specs(bad)).unwrap_err();
        assert!(
            error.contains("실행 가능한 일반 파일"),
            "{command}: {error}"
        );
    }
    // 없는 폴더·일반 파일 루트
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
    // 판본은 읽기만 한 계산과 같고, 설치본은 <기준>/<판본>이다
    assert_eq!(installed.revision, revision_of_spec(&spec));
    assert_eq!(installed.dir, base.join(&installed.revision.0));
    assert_eq!(installs.revisions()["agenda"], installed.revision);
    assert_eq!(installs.aliases().collect::<Vec<_>>(), ["agenda"]);
    // 기준 폴더는 비공개, 설치본은 읽기 전용(폴더 0500, 실행 파일 0500, 일반 파일 0400)
    let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o777;
    assert_eq!(mode(&base), 0o700);
    assert_eq!(mode(&installed.dir), 0o500);
    assert_eq!(mode(&installed.dir.join("bin")), 0o500);
    assert_eq!(mode(&installed.dir.join("bin/server")), 0o500);
    assert_eq!(mode(&installed.dir.join("data.txt")), 0o400);
    assert!(std::fs::write(installed.dir.join("data.txt"), "변조").is_err());
    // 명령은 설치본 안의 절대 경로이고, 실제로 그 복사본이 실행된다
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
    assert_eq!(ran(&installed.argv), "서버 --stdio");
    // 원래 폴더를 바꿔도 설치본은 그대로이고 같은 판본으로 실행된다(원래 경로를 다시 열지 않는다)
    write(
        &source.join("bin/server"),
        "#!/bin/sh\necho 바뀐 서버\n",
        0o755,
    );
    assert_eq!(ran(&installed.argv), "서버 --stdio");
    // 바뀐 원본은 다른 판본의 새 설치본이 되고, 옛 설치본은 남는다
    let changed = install(&specs(spec), &base, uid()).unwrap();
    let new = changed.get("agenda").unwrap();
    assert_ne!(new.revision, installed.revision);
    assert!(installed.dir.is_dir() && new.dir.is_dir());
    assert_eq!(ran(&new.argv), "바뀐 서버");
}

/// 설치 기준 폴더의 항목 이름들(임시·치운 폴더가 남았는지 볼 때 쓴다)
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
    // 같은 내용은 그대로 다시 쓴다(여러 프로세스가 공유한다: 둘 다 살아 있어도 된다)
    let again = install(&specs(spec.clone()), &base, uid()).unwrap();
    assert_eq!(first.get("agenda"), again.get("agenda"));
    assert_eq!(inode(&installed.dir), original_inode, "다시 썼다");
    assert_eq!(
        names(&base),
        std::slice::from_ref(&installed.revision.0),
        "임시 폴더가 남지 않는다"
    );
    // 어긋난 설치본은 쓰는 쪽이 모두 사라진 뒤에만 복구한다
    drop((first, again));

    let data = installed.dir.join("data.txt");
    let unlock = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
    };
    // 바이트가 바뀐 설치본은 다시 쓰지 않는다: 새로 설치해 원래 내용을 되돌리고 어긋난 것은 치운다
    unlock(&installed.dir, 0o700);
    unlock(&data, 0o600);
    std::fs::write(&data, "변조").unwrap();
    let healed = install(&specs(spec.clone()), &base, uid()).unwrap();
    assert_eq!(healed.get("agenda").unwrap().revision, installed.revision);
    assert_eq!(std::fs::read_to_string(&data).unwrap(), "도움말");
    assert_ne!(inode(&installed.dir), original_inode, "새 복사본이다");
    assert_eq!(
        names(&base),
        std::slice::from_ref(&installed.revision.0),
        "어긋난 것은 치웠다"
    );
    drop(healed);

    // 바이트는 같아도 쓸 수 있게 열려 있으면(파일·폴더·루트) 다시 쓰지 않고 새로 설치한다
    for (what, path, mode) in [
        ("파일", data.clone(), 0o600),
        ("폴더", installed.dir.join("bin"), 0o700),
        ("루트", installed.dir.clone(), 0o700),
        ("그룹 쓰기", data.clone(), 0o460),
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
            "{what}: 새 복사본이어야 한다"
        );
        let meta = |p: &Path| std::fs::metadata(p).unwrap().mode() & 0o777;
        assert_eq!(meta(&data), 0o400, "{what}: 읽기 전용이 되돌아왔다");
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
    // 실행 중인 쪽(설치본을 쓰는 동안 공유 잠금을 쥔다)
    let running = install(&specs(spec.clone()), &base, uid()).unwrap();
    let installed = running.get("agenda").unwrap().clone();
    let data = installed.dir.join("data.txt");
    // 그 사이 설치본의 권한이 어긋났다(쓸 수 있게 열림)
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o600)).unwrap();
    let before = inode(&installed.dir);
    // 새 기동은 복구하지 않고 거절한다 — 실행 중인 도구가 자기 파일을 잃지 않는다
    let error = install(&specs(spec.clone()), &base, uid()).unwrap_err();
    assert!(error.contains("다른 프로세스가 쓰는 중"), "{error}");
    assert_eq!(inode(&installed.dir), before, "옛 폴더를 지우지 않았다");
    assert_eq!(std::fs::read_to_string(&data).unwrap(), "도움말");
    assert!(
        std::process::Command::new(&installed.argv[0])
            .output()
            .unwrap()
            .status
            .success(),
        "실행 중인 서버가 자기 파일을 그대로 읽고 실행한다"
    );
    assert_eq!(
        names(&base),
        std::slice::from_ref(&installed.revision.0),
        "거절한 시작이 임시 폴더를 남기지 않는다"
    );
    // 쓰는 쪽이 모두 사라지면 판본을 독점할 수 있으니 복구한다
    drop(running);
    let healed = install(&specs(spec), &base, uid()).unwrap();
    assert_eq!(healed.get("agenda").unwrap().revision, installed.revision);
    assert_ne!(inode(&installed.dir), before, "새 복사본으로 복구했다");
    assert_eq!(
        std::fs::metadata(&data).unwrap().mode() & 0o777,
        0o400,
        "읽기 전용이 되돌아왔다"
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
    // 다른 사용자 소유로 보이는 설치본은 거절한다(루트부터)
    let error = walk(dir, None, Some(uid().wrapping_add(1))).unwrap_err();
    assert!(error.contains("소유자·쓰기 권한"), "{error}");
    // 설치 직전의 원본 폴더는 쓸 수 있으므로 감사를 통과하지 못한다(설치본이 아니다)
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
        "임시 폴더도 남지 않는다"
    );
    // 다른 사용자가 읽을 수 있는 기준 폴더는 거절한다
    let open = dir.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    spec.command = vec!["bin/server".into()];
    let error = install(&specs(spec.clone()), &open, uid()).unwrap_err();
    assert!(error.contains("0700"), "{error}");
    // 다른 사용자 소유로 보이는 기준 폴더도 거절한다
    let error = install(&specs(spec), &base, uid().wrapping_add(1)).unwrap_err();
    assert!(error.contains("본인 소유"), "{error}");
}

#[test]
fn stale_staging_folders_are_swept_but_fresh_ones_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("installs");
    install(&BTreeMap::new(), &base, uid()).unwrap();
    for name in [".staging-old", ".staging-new"] {
        let staging = base.join(name);
        write(&staging.join("x/file"), "남은 것", 0o400);
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
        "오래된 임시 폴더는 치운다"
    );
    assert!(
        base.join(".staging-new").exists(),
        "방금 만든 것은 둔다(다른 시작이 쓰는 중일 수 있다)"
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
