use super::*;
use tokio::time::sleep;

mod env;
mod reap;
mod state;
use crate::{
    sandbox::{Limits, ProxyEndpoint},
    sys::OsEntropy,
    testing::CountingEntropy,
};
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 시험용 가짜 srt. `--version`에 답하고 `--settings <파일> -- 명령…`을 받아 명령을 자식으로 돌린다(exec하지 않아 선두가 srt처럼 남는다).
pub(super) fn fake_srt(dir: &Path, version: &str) -> PathBuf {
    let path = dir.join(format!("fake-srt-{}", safe(version)));
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version}'; exit 0; fi\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --settings) settings=\"$2\"; env > \"$settings.launcher-env\"; shift 2 ;;\n    --) shift; break ;;\n    *) shift ;;\n  esac\ndone\n\"$@\"\nexit $?\n"
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

pub(super) const SERVER: &str = "env > \"$CLAUDE_CODE_TMPDIR/env.txt\"; sleep 300 & echo $! > \"$CLAUDE_CODE_TMPDIR/bg.pid\"; exec cat";

pub(super) fn config(dir: &Path, binary: PathBuf) -> SrtConfig {
    SrtConfig {
        binary,
        work_root: dir.join("work"),
        servers: [(
            "demo-agenda".to_owned(),
            ServerCommand {
                argv: vec!["/bin/sh".into(), "-c".into(), SERVER.into()],
                revision: "img1".into(),
                install_dir: dir.join("install"),
                state: bot_core::install::ServerState::Ephemeral,
            },
        )]
        .into(),
        deny_read: vec![dir.join("home"), dir.join("profiles")],
        path: "/usr/bin:/bin".into(),
        terminate_wait: Duration::from_secs(5),
        host_tools: None,
    }
}

pub(super) fn spec(owner: &str, egress: bool) -> SandboxSpec {
    SandboxSpec {
        owner: owner.into(),
        server: "demo-agenda".into(),
        image: "img1".into(),
        env: vec![
            ("TOKEN".into(), "sekret-value-1".into()),
            ("HTTPS_PROXY".into(), "http://bot:pw@127.0.0.1:9999".into()),
        ],
        egress: egress.then(|| ProxyEndpoint {
            addr: "127.0.0.1:9999".parse().unwrap(),
            url: "http://bot:pw@127.0.0.1:9999".into(),
        }),
        host_env: vec![],
        egress_hosts: if egress {
            vec!["api.example:443".into()]
        } else {
            vec![]
        },
        effects: false,
        limits: Limits {
            wall: Duration::from_secs(60),
        },
    }
}

pub(super) async fn open(dir: &Path) -> SrtSandbox {
    let binary = fake_srt(dir, PINNED_VERSION);
    SrtSandbox::open(config(dir, binary), Arc::new(CountingEntropy::default()))
        .await
        .unwrap()
}

pub(super) async fn wait_for(path: &Path) -> String {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        sleep(Duration::from_millis(25)).await;
    }
    panic!("파일이 생기지 않았다: {}", path.display());
}

pub(super) fn one_file(dir: &Path, ext: &str) -> PathBuf {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    found.remove(0)
}

/// `.srt` 안에 표식(`.done`) 말고 남은 것(설정·pid 기록).
pub(super) fn leftovers(root: &Path) -> Vec<String> {
    std::fs::read_dir(root.join("work/.srt"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.ends_with(".done") && !n.ends_with(".launcher-env"))
        .collect()
}

pub(super) fn work_dir(root: &Path) -> PathBuf {
    std::fs::read_dir(root.join("work"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.file_name().unwrap() != ".srt")
        .unwrap()
}

/// `pgrep`을 대신하는 가짜: 종료 코드만 돌려준다.
pub(super) fn fake_tool(dir: &Path, name: &str, code: i32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\nexit {code}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn it_refuses_to_open_without_srt_or_with_another_version_and_accepts_the_pinned_one() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = SrtSandbox::open(
        config(tmp.path(), tmp.path().join("nope")),
        Arc::new(OsEntropy),
    )
    .await;
    assert!(matches!(missing, Err(Error::Unavailable)), "srt가 없다");
    for version in ["0.0.1", "0.0.84", "garbage", ""] {
        let binary = fake_srt(tmp.path(), version);
        let opened = SrtSandbox::open(config(tmp.path(), binary), Arc::new(OsEntropy)).await;
        assert!(matches!(opened, Err(Error::Unavailable)), "{version:?}");
    }
    for version in ["0.0.83", "srt 0.0.83", "v0.0.83"] {
        let binary = fake_srt(tmp.path(), version);
        assert!(
            SrtSandbox::open(config(tmp.path(), binary), Arc::new(OsEntropy))
                .await
                .is_ok(),
            "{version}"
        );
    }
}

#[tokio::test]
async fn it_runs_only_registered_servers_at_the_approved_revision_and_no_effects_channel() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let mut unknown = spec("r1.1.c1", false);
    unknown.server = "other".into();
    assert!(matches!(sandbox.spawn(unknown).await, Err(Error::Denied)));
    let mut changed = spec("r1.1.c1", false);
    changed.image = "img2".into();
    assert!(
        matches!(sandbox.spawn(changed).await, Err(Error::Denied)),
        "승인 뒤 설치 판본이 바뀌었다"
    );
    let mut effects = spec("r1.1.c1", false);
    effects.effects = true;
    assert!(matches!(
        sandbox.spawn(effects).await,
        Err(Error::Unavailable)
    ));
    assert!(
        std::fs::read_dir(tmp.path().join("work"))
            .unwrap()
            .flatten()
            .all(|e| e.file_name() == ".srt"),
        "아무것도 만들지 않았다"
    );
}

#[tokio::test]
async fn the_settings_are_private_live_outside_the_work_dir_and_follow_the_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let process = sandbox.spawn(spec("r1.1.c1", true)).await.unwrap();
    let meta = tmp.path().join("work/.srt");
    let path = one_file(&meta, "json");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let work = std::fs::read_dir(tmp.path().join("work"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.file_name().unwrap() != ".srt")
        .unwrap();
    assert_eq!(
        std::fs::metadata(&work).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(
        !path.starts_with(&work),
        "설정 파일은 도구가 쓸 수 있는 폴더 밖에 있다"
    );
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value["network"]["allowedDomains"],
        json!(["api.example:443"])
    );
    assert_eq!(
        value["network"]["parentProxy"],
        json!({"http": "http://bot:pw@127.0.0.1:9999", "https": "http://bot:pw@127.0.0.1:9999"})
    );
    assert_eq!(
        (
            value["network"]["strictAllowlist"].clone(),
            value["network"]["allowLocalBinding"].clone()
        ),
        (json!(true), json!(false))
    );
    let (work_s, install_s) = (
        work.display().to_string(),
        tmp.path().join("install").display().to_string(),
    );
    assert_eq!(
        value["filesystem"]["allowWrite"],
        json!([work_s]),
        "쓰기는 이 호출의 작업 폴더뿐"
    );
    assert_eq!(value["filesystem"]["allowRead"], json!([work_s, install_s]));
    let deny: Vec<String> =
        serde_json::from_value(value["filesystem"]["denyRead"].clone()).unwrap();
    assert!(
        deny.contains(&tmp.path().join("home").display().to_string())
            && deny.contains(&tmp.path().join("work").display().to_string())
    );
    assert!(!text.contains("sekret-value-1"), "비밀은 설정 파일에 없다");
    Box::new(process.handle).terminate().await.unwrap();
}

#[tokio::test]
async fn without_a_declared_host_the_settings_allow_no_network_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let process = sandbox.spawn(spec("r1.1.c1", false)).await.unwrap();
    let value: Value = serde_json::from_str(
        &std::fs::read_to_string(one_file(&tmp.path().join("work/.srt"), "json")).unwrap(),
    )
    .unwrap();
    assert_eq!(value["network"]["allowedDomains"], json!([]));
    assert!(value["network"].get("parentProxy").is_none());
    Box::new(process.handle).terminate().await.unwrap();
}

#[tokio::test]
async fn stdio_reaches_the_child_and_back() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let mut process = sandbox.spawn(spec("r1.1.c1", false)).await.unwrap();
    process
        .stdio
        .write
        .write_all(b"{\"jsonrpc\":\"2.0\"}\n")
        .await
        .unwrap();
    process.stdio.write.flush().await.unwrap();
    let mut line = String::new();
    timeout(
        Duration::from_secs(5),
        BufReader::new(&mut process.stdio.read).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(line, "{\"jsonrpc\":\"2.0\"}\n");
    Box::new(process.handle).terminate().await.unwrap();
}

#[tokio::test]
async fn terminate_ends_the_whole_group_and_removes_every_trace() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let process = sandbox.spawn(spec("r1.1.c1", false)).await.unwrap();
    let work = std::fs::read_dir(tmp.path().join("work"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.file_name().unwrap() != ".srt")
        .unwrap();
    let background: u32 = wait_for(&work.join("bg.pid")).await.trim().parse().unwrap();
    assert!(
        std::process::Command::new("/bin/kill")
            .args(["-s", "0", &background.to_string()])
            .status()
            .unwrap()
            .success(),
        "자식이 띄운 백그라운드 프로세스가 살아 있다"
    );
    Box::new(process.handle).terminate().await.unwrap();
    // 자식이 띄운 백그라운드 프로세스도 같은 그룹이라 함께 사라진다
    let alive = std::process::Command::new("/bin/kill")
        .args(["-s", "0", &background.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive, "그룹 전체가 끝나야 한다");
    assert!(!work.exists());
    assert!(
        leftovers(tmp.path()).is_empty(),
        "설정·pid 기록은 지운다(표식만 남긴다): {:?}",
        leftovers(tmp.path())
    );
    assert!(sandbox.live.lock().unwrap().is_empty());
}

#[test]
fn the_settings_function_is_a_pure_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path(), PathBuf::from("srt"));
    let s = spec("o", true);
    let a = settings(&s, Path::new("/w/one"), Path::new("/i"), &cfg);
    assert_eq!(a, settings(&s, Path::new("/w/one"), Path::new("/i"), &cfg));
    assert_eq!(a["filesystem"]["allowWrite"], json!(["/w/one"]));
    assert_eq!(
        a["filesystem"]["denyWrite"],
        json!(["/tmp/claude", "/private/tmp/claude"])
    );
}
