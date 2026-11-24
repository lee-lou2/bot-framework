//! 환경·정책: 런처 환경 고정, 이름 거절, 판본 상수, 공용 임시 폴더.

use super::*;

#[tokio::test]
async fn the_child_environment_is_only_what_was_declared() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let process = sandbox.spawn(spec("r1.1.c1", true)).await.unwrap();
    let work = std::fs::read_dir(tmp.path().join("work"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.file_name().unwrap() != ".srt")
        .unwrap();
    let env = wait_for(&work.join("env.txt")).await;
    let lines: std::collections::BTreeSet<&str> = env
        .lines()
        .filter(|l| {
            !l.starts_with("PWD=")
                && !l.starts_with("SHLVL=")
                && !l.starts_with('_')
                && !l.starts_with("OLDPWD=")
        })
        .collect();
    let expected: std::collections::BTreeSet<String> = [
        "PATH=/usr/bin:/bin".to_owned(),
        format!("CLAUDE_CODE_TMPDIR={}", work.display()),
        "TOKEN=sekret-value-1".to_owned(),
    ]
    .into();
    assert_eq!(
        lines
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>(),
        expected,
        "{env}"
    );
    Box::new(process.handle).terminate().await.unwrap();
}

#[tokio::test]
async fn the_launcher_environment_is_host_fixed_and_credentials_ride_fixed_carrier_names() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let process = sandbox.spawn(spec("r1.1.c1", true)).await.unwrap();
    let launcher = wait_for(
        &one_file(&tmp.path().join("work/.srt"), "json").with_extension("json.launcher-env"),
    )
    .await;
    let names: std::collections::BTreeSet<&str> = launcher
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .filter(|k| !matches!(*k, "PWD" | "SHLVL" | "_" | "OLDPWD"))
        .collect();
    assert_eq!(
        names,
        ["BOT_CARRY_0", "CLAUDE_CODE_TMPDIR", "PATH"].into(),
        "런처 환경은 이것뿐이다: {launcher}"
    );
    assert!(
        !launcher.contains("TOKEN="),
        "선언한 이름은 런처에 올라가지 않는다"
    );
    // 자식 안에서는 선언한 이름으로 옮겨 달리고 운반 변수는 없다
    let env = wait_for(&work_dir(tmp.path()).join("env.txt")).await;
    assert!(
        env.lines().any(|l| l == "TOKEN=sekret-value-1") && !env.contains("BOT_CARRY"),
        "{env}"
    );
    Box::new(process.handle).terminate().await.unwrap();
}

#[tokio::test]
async fn credential_names_that_could_steer_the_launcher_or_the_shell_are_refused_before_anything_is_created()
 {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    for name in [
        "PATH",
        "HOME",
        "NODE_OPTIONS",
        "NODE_PATH",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "BASH_ENV",
        "ENV",
        "IFS",
        "SHELLOPTS",
        "PS4",
        "TMPDIR",
        "CLAUDE_CODE_TMPDIR",
        "SRT_DEBUG",
        "BOT_CARRY_0",
        "BOT_SECRET",
        "MY_PROXY",
        "NPM_CONFIG_PREFIX",
        "lowercase",
        "1BAD",
        "A-B",
        "",
        "WITH SPACE",
        "A;rm -rf",
    ] {
        let mut bad = spec("r1.1.c1", false);
        bad.env = vec![(name.into(), "value".into())];
        assert!(
            matches!(sandbox.spawn(bad).await, Err(Error::Denied)),
            "{name:?}"
        );
    }
    let mut nul = spec("r1.1.c1", false);
    nul.env = vec![("TOKEN".into(), "a\0b".into())];
    assert!(matches!(sandbox.spawn(nul).await, Err(Error::Denied)));
    let entries: Vec<_> = std::fs::read_dir(tmp.path().join("work"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != ".srt")
        .collect();
    assert!(entries.is_empty(), "거절하면 작업 폴더도 만들지 않는다");
    assert!(
        std::fs::read_dir(tmp.path().join("work/.srt"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        !forbidden_env_name("API_TOKEN")
            && !forbidden_env_name("CALENDAR_TOKEN")
            && !forbidden_env_name("_X")
    );
}

#[tokio::test]
async fn the_version_is_compared_to_the_pinned_constant_only() {
    let tmp = tempfile::tempdir().unwrap();
    // 설정으로 판본을 바꿀 수 있는 입구가 없다: 다른 판본의 srt는 어떤 설정으로도 열리지 않는다
    for version in ["0.0.82", "0.0.84", "1.0.0", "0.0.83-rc1"] {
        let binary = fake_srt(tmp.path(), version);
        assert!(
            matches!(
                SrtSandbox::open(config(tmp.path(), binary), Arc::new(OsEntropy)).await,
                Err(Error::Unavailable)
            ),
            "{version}"
        );
    }
}

#[tokio::test]
async fn the_shared_tmp_dir_is_closed_for_writing_too() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path(), PathBuf::from("srt"));
    let value = settings(
        &spec("o", false),
        Path::new("/w/one"),
        Path::new("/i"),
        &cfg,
    );
    assert_eq!(
        value["filesystem"]["denyWrite"],
        json!(["/tmp/claude", "/private/tmp/claude"])
    );
    assert_eq!(value["filesystem"]["allowWrite"], json!(["/w/one"]));
}

#[tokio::test]
async fn host_provided_variables_reach_the_child_through_carriers_only_when_allowed() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let mut allowed = spec("r1.1.c1", false);
    allowed.host_env = vec![(
        "BOT_EFFECT_URL".into(),
        "http://bot-effects.invalid/effect".into(),
    )];
    let process = sandbox.spawn(allowed).await.unwrap();
    let env = wait_for(&work_dir(tmp.path()).join("env.txt")).await;
    assert!(
        env.lines()
            .any(|l| l == "BOT_EFFECT_URL=http://bot-effects.invalid/effect")
            && env.lines().any(|l| l == "TOKEN=sekret-value-1"),
        "{env}"
    );
    let launcher = wait_for(
        &one_file(&tmp.path().join("work/.srt"), "json").with_extension("json.launcher-env"),
    )
    .await;
    assert!(
        !launcher.contains("BOT_EFFECT_URL=") && launcher.contains("BOT_CARRY_1="),
        "런처에는 운반 변수만: {launcher}"
    );
    Box::new(process.handle).terminate().await.unwrap();
    // 허용한 이름이 아니면 띄우지 않는다
    for name in ["BOT_OTHER", "PATH", "NODE_OPTIONS", "BOT_CARRY_0"] {
        let mut bad = spec("r1.1.c2", false);
        bad.host_env = vec![(name.into(), "x".into())];
        assert!(
            matches!(sandbox.spawn(bad).await, Err(Error::Denied)),
            "{name}"
        );
    }
}
