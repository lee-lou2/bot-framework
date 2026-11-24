//! 실제 srt로 도구 서버 샌드박스의 격리 보장을 증명하는 카나리. 가짜 어댑터 시험과 별개다.
//!
//! 실행: `BOT_SRT_BIN=<srt 경로> BOT_SRT_CANARY=required cargo test -p bot-broker --features testing --test srt_canary -- --test-threads=1`
//! - `BOT_SRT_CANARY=required`이면 srt가 없거나 판본이 다를 때 시험이 실패한다(CI의 필수 모드).
//! - 설정하지 않으면 srt가 없을 때 건너뛴다(개발 PC).
//!
//! macOS는 Seatbelt, Linux는 bubblewrap으로 같은 시험이 돌아야 한다.

#![cfg(all(unix, feature = "testing"))]

use bot_broker::{
    proxy::{Connector, Resolver},
    sandbox::{Limits, Pipe, Process, ProxyEndpoint, Sandbox, SandboxSpec},
    srt::{PINNED_VERSION, ServerCommand, SrtConfig, SrtSandbox},
    sys::OsEntropy,
    testing::{FakeConnector, FakeResolver},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    time::timeout,
};

struct Canary {
    sandbox: SrtSandbox,
    dir: tempfile::TempDir,
}

fn required() -> bool {
    std::env::var("BOT_SRT_CANARY").is_ok_and(|v| v == "required")
}

/// srt가 쓸 수 있으면 어댑터를 연다. 없으면 필수 모드는 실패, 아니면 `None`(건너뜀).
async fn canary(command: &str) -> Option<Canary> {
    let dir = tempfile::tempdir().unwrap();
    let binary = std::env::var_os("BOT_SRT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("srt"));
    let install = dir.path().join("install");
    std::fs::create_dir_all(&install).unwrap();
    let cfg = SrtConfig {
        binary,
        work_root: dir.path().join("work"),
        servers: [(
            "canary".to_owned(),
            ServerCommand {
                argv: vec!["/bin/sh".into(), "-c".into(), command.into()],
                revision: "img1".into(),
                install_dir: install,
                state: bot_core::install::ServerState::Ephemeral,
            },
        )]
        .into(),
        deny_read: vec![dir.path().join("home")],
        path: std::env::var("PATH").unwrap_or_default(),
        terminate_wait: Duration::from_secs(10),
        host_tools: None,
    };
    match SrtSandbox::open(cfg, Arc::new(OsEntropy)).await {
        Ok(sandbox) => Some(Canary { sandbox, dir }),
        Err(_) if required() => {
            panic!("필수 모드: srt {PINNED_VERSION}를 쓸 수 없다(BOT_SRT_BIN 확인)")
        }
        Err(_) => {
            eprintln!("srt가 없어 건너뛴다(BOT_SRT_CANARY=required로 실패시킬 수 있다)");
            None
        }
    }
}

fn spec(
    owner: &str,
    env: &[(&str, &str)],
    egress: Option<(ProxyEndpoint, Vec<String>)>,
) -> SandboxSpec {
    let (endpoint, hosts) = match egress {
        Some((e, h)) => (Some(e), h),
        None => (None, vec![]),
    };
    SandboxSpec {
        owner: owner.into(),
        server: "canary".into(),
        image: "img1".into(),
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        host_env: vec![],
        egress: endpoint,
        egress_hosts: hosts,
        effects: false,
        limits: Limits {
            wall: Duration::from_secs(60),
        },
    }
}

/// 자식이 한 줄을 쓸 때까지 읽는다.
async fn line(pipe: &mut Pipe) -> String {
    let mut text = String::new();
    timeout(
        Duration::from_secs(20),
        BufReader::new(&mut pipe.read).read_line(&mut text),
    )
    .await
    .expect("자식이 답하지 않았다")
    .unwrap();
    text.trim().to_owned()
}

async fn run_and_read(c: &Canary, spec: SandboxSpec) -> (Process, String) {
    let mut process = c
        .sandbox
        .spawn(spec)
        .await
        .expect("srt 샌드박스를 띄우지 못했다");
    let first = line(&mut process.stdio).await;
    (process, first)
}

async fn end(process: Process) {
    Box::new(process.handle)
        .terminate()
        .await
        .expect("프로세스 그룹이 끝났음을 확인하지 못했다");
}

fn exists(path: &Path) -> bool {
    path.exists()
}

#[tokio::test]
async fn the_tool_sees_only_declared_secrets_and_no_host_environment() {
    let Some(c) = canary("env | sort | tr '\\n' ';'; echo; exec cat").await else {
        return;
    };
    let (process, seen) =
        run_and_read(&c, spec("r1.1.c1", &[("TOKEN", "sekret-canary-1")], None)).await;
    assert!(seen.contains("TOKEN=sekret-canary-1"), "{seen}");
    for host_only in [
        "HOME=",
        "USER=",
        "LOGNAME=",
        "SHELL=",
        "SSH_AUTH_SOCK=",
        "TERM_PROGRAM=",
    ] {
        assert!(
            !seen.contains(host_only),
            "호스트 환경 {host_only}이 샌드박스로 새었다: {seen}"
        );
    }
    // 우리가 준 것(선언한 자격 + 작업 폴더 경로)과 srt가 더하는 자기 프록시 주소 말고는 없다. 우리 프록시 자격은 따로 열지 않았으니 없다.
    assert!(
        !seen.contains("canary-pass") && !seen.contains("bot:"),
        "브로커 프록시 자격이 새었다: {seen}"
    );
    let names: Vec<&str> = seen
        .split(';')
        .filter_map(|kv| kv.split_once('=').map(|(k, _)| k))
        .collect();
    for ours in ["TOKEN", "PATH"] {
        assert!(names.contains(&ours), "{ours}가 없다: {names:?}");
    }
    end(process).await;
}

#[tokio::test]
async fn without_a_declared_host_the_tool_cannot_reach_a_listening_host_service() {
    let Some(c) = canary("bash -c 'exec 3<>/dev/tcp/127.0.0.1/${CANARY_PORT}' 2>/dev/null && echo CONNECTED || echo BLOCKED; exec cat").await else { return };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let (process, answer) =
        run_and_read(&c, spec("r1.1.c2", &[("CANARY_PORT", &port)], None)).await;
    assert_eq!(answer, "BLOCKED", "허용한 호스트가 없으면 네트워크는 0이다");
    end(process).await;
}

#[tokio::test]
async fn a_declared_host_goes_through_our_proxy_and_everything_else_is_refused() {
    // curl이 srt의 프록시(허용 도메인 = 선언한 호스트)를 거쳐 우리 프록시(주소 검사)까지 가는지 본다
    let Some(c) = canary("for h in allowed.test evil.test; do curl -sS -o /dev/null --max-time 10 https://$h/ >/dev/null 2>&1; echo \"$h $?\"; done | tr '\\n' ';'; echo; exec cat").await else { return };
    let resolver: Arc<dyn Resolver> = Arc::new(FakeResolver::new(&[
        ("allowed.test", &["93.184.216.34"]),
        ("evil.test", &["93.184.216.35"]),
    ]));
    let connector = Arc::new(FakeConnector::new());
    let policy = bot_broker::egress::EgressPolicy::new(
        &[bot_broker::resolve::HostPort {
            host: "allowed.test".into(),
            port: 443,
        }],
        None,
    );
    let proxy_cfg = bot_broker::proxy::ProxyConfig::new(policy, "bot".into(), "canary-pass".into());
    let proxy = bot_broker::proxy::Proxy::start(
        proxy_cfg,
        resolver,
        connector.clone() as Arc<dyn Connector>,
    )
    .await
    .unwrap();
    let endpoint = ProxyEndpoint {
        addr: proxy.addr(),
        url: format!("http://bot:canary-pass@{}", proxy.addr()),
    };
    let (process, report) = run_and_read(
        &c,
        spec(
            "r1.1.c3",
            &[],
            Some((endpoint, vec!["allowed.test:443".into()])),
        ),
    )
    .await;
    assert!(
        report.contains("allowed.test ") && report.contains("evil.test "),
        "{report}"
    );
    // 선언한 호스트만 우리 프록시까지 왔고(주소는 검사한 공개 주소에 고정), 선언하지 않은 호스트는 접속 시도조차 없다
    assert_eq!(
        connector.addrs(),
        vec!["93.184.216.34:443".parse().unwrap()],
        "{report}"
    );
    end(process).await;
    proxy.stop().await;
}

#[tokio::test]
async fn reads_of_host_secrets_and_other_runs_are_refused_and_the_install_dir_is_readable() {
    let Some(c) = canary("for f in \"$HOST_SECRET\" \"$OTHER_RUN\" \"$INSTALLED\"; do cat \"$f\" >/dev/null 2>&1 && printf 'read ' || printf 'blocked '; done; echo; exec cat").await else { return };
    let home = c.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("secret.txt"), "TOP-SECRET").unwrap();
    let other = c.dir.path().join("work/other-run");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("data.txt"), "OTHER-RUN-DATA").unwrap();
    let installed = c.dir.path().join("install/server.txt");
    std::fs::write(&installed, "server files").unwrap();
    let env = [
        ("HOST_SECRET", home.join("secret.txt").display().to_string()),
        ("OTHER_RUN", other.join("data.txt").display().to_string()),
        ("INSTALLED", installed.display().to_string()),
    ];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (process, answer) = run_and_read(&c, spec("r1.1.c4", &env, None)).await;
    assert_eq!(
        answer, "blocked blocked read",
        "호스트 홈·다른 run 읽기 0, 서버 설치 폴더는 읽기 가능"
    );
    end(process).await;
}

#[tokio::test]
async fn writes_are_confined_to_the_runs_own_work_dir() {
    let Some(c) = canary("echo ok > \"$TMPDIR/own.txt\" && printf 'own-ok ' || printf 'own-blocked '; echo x > \"$OUTSIDE\" 2>/dev/null && printf 'outside-written' || printf 'outside-blocked'; echo; exec cat").await else { return };
    let outside = c.dir.path().join("outside.txt");
    let (process, answer) = run_and_read(
        &c,
        spec(
            "r1.1.c5",
            &[("OUTSIDE", &outside.display().to_string())],
            None,
        ),
    )
    .await;
    assert_eq!(answer, "own-ok outside-blocked", "쓰기는 run 작업 폴더만");
    assert!(!exists(&outside));
    end(process).await;
}

#[tokio::test]
async fn terminating_ends_the_whole_process_tree() {
    let Some(c) = canary("sleep 300 & echo $! ; exec cat").await else {
        return;
    };
    let (process, pid) = run_and_read(&c, spec("r1.1.c6", &[], None)).await;
    let pid: u32 = pid.parse().expect("자식이 띄운 백그라운드 pid");
    end(process).await;
    let alive = std::process::Command::new("/bin/kill")
        .args(["-s", "0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive, "샌드박스 안에서 띄운 프로세스도 함께 끝나야 한다");
}

#[tokio::test]
async fn a_tool_server_can_speak_stdio_through_the_sandbox() {
    let Some(c) = canary("exec cat").await else {
        return;
    };
    let mut process = c.sandbox.spawn(spec("r1.1.c7", &[], None)).await.unwrap();
    process
        .stdio
        .write
        .write_all(b"{\"jsonrpc\":\"2.0\"}\n")
        .await
        .unwrap();
    process.stdio.write.flush().await.unwrap();
    assert_eq!(line(&mut process.stdio).await, "{\"jsonrpc\":\"2.0\"}");
    end(process).await;
}

#[tokio::test]
async fn the_shared_srt_tmp_dir_is_closed_in_both_directions() {
    // srt는 /tmp/claude를 모든 실행이 공유하는 쓰기 가능한 곳으로 둔다. 우리는 쓰기도 읽기도 막는다.
    let shared = std::path::PathBuf::from("/tmp/claude");
    std::fs::create_dir_all(&shared).unwrap();
    let id = std::process::id();
    let (seed, probe) = (
        shared.join(format!("seed-{id}.txt")),
        shared.join(format!("write-probe-{id}.txt")),
    );
    std::fs::write(&seed, "SYNTHETIC-OTHER-RUN-DATA").unwrap();
    let _ = std::fs::remove_file(&probe);
    let command = format!(
        "echo x > \"{}\" && printf 'wrote ' || printf 'write-blocked '; cat \"{}\" >/dev/null 2>&1 && printf 'read' || printf 'read-blocked'; echo; exec cat",
        probe.display(),
        seed.display()
    );
    let Some(c) = canary(&command).await else {
        let _ = std::fs::remove_file(&seed);
        return;
    };
    let (process, answer) = run_and_read(&c, spec("r1.1.c8", &[], None)).await;
    assert_eq!(
        answer, "write-blocked read-blocked",
        "/tmp/claude 쓰기·읽기 모두 막혀야 한다"
    );
    assert!(
        !probe.exists(),
        "샌드박스가 공유 임시 폴더에 파일을 만들었다"
    );
    end(process).await;
    let _ = std::fs::remove_file(&seed);
    let _ = std::fs::remove_file(&probe);
}

#[tokio::test]
async fn names_that_could_steer_the_launcher_are_refused_and_secrets_still_arrive_under_their_names()
 {
    let Some(c) = canary("printf '%s\\n' \"$MY_TOKEN\"; exec cat").await else {
        return;
    };
    for name in [
        "PATH",
        "NODE_OPTIONS",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "BASH_ENV",
    ] {
        assert!(
            c.sandbox
                .spawn(spec("r1.1.c9", &[(name, "x")], None))
                .await
                .is_err(),
            "{name}"
        );
    }
    let (process, got) = run_and_read(
        &c,
        spec("r1.1.c10", &[("MY_TOKEN", "carried-secret-value")], None),
    )
    .await;
    assert_eq!(
        got, "carried-secret-value",
        "운반 변수로 실려 선언한 이름으로 도착한다"
    );
    end(process).await;
}

/// 전용 주소로 온 프레임을 기록하고 고정 응답을 돌려주는 처리기.
struct Recorder(std::sync::Mutex<Vec<String>>);

impl bot_broker::proxy::LocalService for Recorder {
    fn handle(&self, frame: Vec<u8>) -> bot_core::port::Reply<'_, Vec<u8>> {
        self.0
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&frame).into_owned());
        Box::pin(async {
            Ok(br#"{"v":1,"id":"1","state":"started","effect":"e-1","send":true}"#.to_vec())
        })
    }
}

#[tokio::test]
async fn a_tool_in_srt_reports_steps_through_the_proxys_local_address_without_any_other_network() {
    // curl이 srt의 프록시를 거쳐 우리 프록시의 전용 주소로 단계 원장 프레임을 보내고 응답을 받는다
    let Some(c) = canary("curl -sS --max-time 15 -X POST \"$BOT_EFFECT_URL\" -d '{\"v\":1,\"id\":\"1\",\"method\":\"effect.request\",\"step\":\"upload\",\"args\":{}}' 2>&1; echo; curl -sS --max-time 8 -o /dev/null -w 'outside=%{http_code}' https://outside.test/ 2>&1 | tail -c 120; echo; exec cat").await else { return };
    let recorder = Arc::new(Recorder(Default::default()));
    let mut cfg = bot_broker::proxy::ProxyConfig::new(
        bot_broker::egress::EgressPolicy::new(&[], None),
        "bot".into(),
        "canary-pass".into(),
    );
    cfg.local = Some(recorder.clone());
    let connector = Arc::new(FakeConnector::new());
    let proxy = bot_broker::proxy::Proxy::start(
        cfg,
        Arc::new(FakeResolver::new(&[])),
        connector.clone() as Arc<dyn Connector>,
    )
    .await
    .unwrap();
    let endpoint = ProxyEndpoint {
        addr: proxy.addr(),
        url: format!("http://bot:canary-pass@{}", proxy.addr()),
    };
    let mut spec = spec(
        "r1.1.c11",
        &[],
        Some((endpoint, vec!["bot-effects.invalid:80".into()])),
    );
    spec.host_env = vec![(
        "BOT_EFFECT_URL".into(),
        "http://bot-effects.invalid/effect".into(),
    )];
    let mut process = c
        .sandbox
        .spawn(spec)
        .await
        .expect("srt 샌드박스를 띄우지 못했다");
    let first = line(&mut process.stdio).await;
    let second = line(&mut process.stdio).await;
    assert!(
        first.contains(r#""state":"started""#) && first.contains(r#""send":true"#),
        "전용 주소 응답이 도구에 닿았다: {first:?}"
    );
    assert_eq!(
        recorder.0.lock().unwrap().len(),
        1,
        "프레임은 처리기에 한 번 닿았다"
    );
    assert!(recorder.0.lock().unwrap()[0].contains("effect.request"));
    assert!(
        !second.contains("outside=200"),
        "선언하지 않은 호스트는 열리지 않는다: {second:?}"
    );
    assert!(connector.addrs().is_empty(), "바깥으로 나간 접속은 없다");
    end(process).await;
    proxy.stop().await;
}
