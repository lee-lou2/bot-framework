//! 진짜 Apple `container` 카나리 공통 도구: CLI·서비스·로컬 이미지 확인, 시험 전용 이름의 망·인스턴스, 정리, 모델 명세.
//! 만들고 지우는 것은 이 시험의 `botvm-c<pid>*` 컨테이너와 `botvm-canary-<pid>*` 망뿐이다(다른 프로젝트 것은 건드리지 않는다).
#![allow(dead_code)]

#[allow(unused_imports)]
pub use bot_core::{
    id::*,
    port::Error,
    sandbox::{ModelProcess, ModelSpec, Mount, Sandbox},
};
#[allow(unused_imports)]
pub use bot_runtime::model::container::{
    Anchored, ContainerConfig, ContainerSandbox, Entry, Health, StartError,
};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

pub const IMAGE: &str = "alpine:3.20";

fn container_bin() -> Option<PathBuf> {
    let found = ["/opt/homebrew/bin/container", "/usr/local/bin/container"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists());
    found.or_else(|| {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join("container"))
                .find(|p| p.exists())
        })
    })
}

pub fn ok(program: &Path, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 이 기계에서 도는 실제 VM 시험(다른 역할의 실행 포함)은 한 번에 하나만 돈다: 서로의 VM 시작·망을 흔들어 시험이 가끔 실패하는 일을 막는다.
/// 같은 프로세스 안에서는 시험들이 차례로 돌고, 다른 프로세스와는 기계 전체 잠금 파일로 겹치지 않는다(이 시험 파일이 도는 동안 쥐고, 끝나면 운영체제가 놓는다).
pub async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    static MACHINE: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();
    let guard = ONE.lock().await;
    if MACHINE.get().is_none() {
        let file = tokio::task::spawn_blocking(machine_lock).await.unwrap();
        let _ = MACHINE.set(file);
    }
    guard
}

fn machine_lock() -> std::fs::File {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/bot-vm-canary.lock")
        .expect("VM 시험 잠금 파일");
    // 다른 시험이 15분 넘게 쥐고 있으면(멈춘 것) 기다리지 않고 진행한다
    for _ in 0..4500 {
        if rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok() {
            return file;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!("warning: 다른 VM 시험이 잠금을 오래 쥐고 있어 잠금 없이 진행한다");
    file
}

/// 쓸 수 있는 환경이면 `container` 경로. 아니면 건너뛴다(`BOT_CONTAINER_CANARY=required`이면 실패한다).
pub fn available() -> Option<PathBuf> {
    let required = std::env::var("BOT_CONTAINER_CANARY").as_deref() == Ok("required");
    let program = container_bin()
        .filter(|p| ok(p, &["system", "status"]) && ok(p, &["image", "inspect", IMAGE]));
    if program.is_none() {
        assert!(
            !required,
            "BOT_CONTAINER_CANARY=required: container CLI·서비스·로컬 {IMAGE} 이미지가 필요하다"
        );
        eprintln!("skipped: container CLI, its service or the local {IMAGE} image is unavailable");
    }
    program
}

/// 운영 모델 이미지(신뢰 init 포함)의 정확한 참조: `BOT_MODEL_IMAGE=이름@sha256:…`(`images/model/build.sh`가 알려 준다). 없으면 건너뛴다.
/// `BOT_CONTAINER_CANARY=required`이면 실패한다. 이미지는 내려받지 않고 로컬에 있어야 한다.
pub fn model_image(program: &Path) -> Option<String> {
    let required = std::env::var("BOT_CONTAINER_CANARY").as_deref() == Ok("required");
    let image = std::env::var("BOT_MODEL_IMAGE")
        .ok()
        .filter(|image| image.contains("@sha256:") && ok(program, &["image", "inspect", image]));
    if image.is_none() {
        assert!(
            !required,
            "BOT_CONTAINER_CANARY=required: BOT_MODEL_IMAGE=<이름@sha256:…> 로컬 이미지가 필요하다(images/model/build.sh)"
        );
        eprintln!("skipped: BOT_MODEL_IMAGE (a local name@sha256:… model image) is not set");
    }
    image
}

/// 실행 중인 컨테이너 한 개의 IPv4(망에 붙은 첫 주소).
pub fn ip_of(program: &Path, id: &str) -> Option<String> {
    let output = Command::new(program)
        .args(["ls", "--format", "json"])
        .output()
        .ok()?;
    let all: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    all.as_array()?.iter().find_map(|c| {
        (c["configuration"]["id"].as_str()? == id).then(|| {
            let address = c["status"]["networks"][0]["ipv4Address"].as_str()?;
            Some(address.split('/').next()?.to_owned())
        })?
    })
}

/// 시험 하나의 시험 전용 이름·폴더와 정리. 패닉해도 이 시험의 컨테이너(접두사)와 망만 지운다.
pub struct Env {
    pub program: PathBuf,
    pub network: String,
    pub instance: String,
    pub root: PathBuf,
}

impl Env {
    pub fn new(program: PathBuf, tag: &str) -> Self {
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("bot-container-{tag}-{pid}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("input")).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        std::fs::write(root.join("input/in.txt"), "읽기 전용 입력").unwrap();
        Self {
            program,
            network: format!("botvm-canary-{pid}-{tag}"),
            instance: format!("botvm-c{pid}{tag}"),
            root,
        }
    }

    /// 이 시험의 호스트 전용 망 부분망: 같은 기계에서 도는 다른 망(다른 시험·다른 프로젝트)과 겹치지 않게 `container`의 자동 배정 범위 밖(192.168.200~249)에서 고른다.
    pub fn subnet(&self) -> String {
        let seed = self.network.bytes().fold(std::process::id(), |h, b| {
            h.wrapping_mul(31).wrapping_add(u32::from(b))
        });
        format!("192.168.{}.0/24", 200 + seed % 50)
    }

    /// 방화벽 진입 없이(이 이미지에는 init이 없다) 시작하는 설정.
    pub fn config(&self) -> ContainerConfig {
        let mut config = ContainerConfig::new(IMAGE, &self.network, &self.root)
            .named(&self.instance)
            .with_lock_dir(self.root.join("locks"))
            .with_subnet(self.subnet());
        config.program = self.program.to_str().unwrap().to_owned();
        config.cpus = 1;
        config.memory_mib = 512;
        config.unfirewalled()
    }

    /// 이미지의 신뢰 init을 쓰는(운영 모양) 설정: 방화벽을 걸고 비root로 고정 `claude`를 띄운다.
    pub fn firewalled(&self, image: &str) -> ContainerConfig {
        let mut config = ContainerConfig::new(image, &self.network, &self.root)
            .named(&self.instance)
            .with_lock_dir(self.root.join("locks"))
            .with_subnet(self.subnet())
            .with_entry(Entry::model_image());
        config.program = self.program.to_str().unwrap().to_owned();
        config.cpus = 1;
        config.memory_mib = 512;
        config
    }

    /// 이 시험의 접두사로 보이는 컨테이너 ID들.
    pub fn ids(&self) -> Vec<String> {
        ls_ids(&self.program, &format!("{}-", self.instance))
    }

    /// 조건이 될 때까지(최대 `secs`초) 기다린다.
    pub async fn until(&self, secs: u64, what: &str, mut done: impl FnMut(&Env) -> bool) {
        for _ in 0..secs * 10 {
            if done(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{what}");
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let listing = Command::new(&self.program)
            .args(["ls", "--all", "--format", "json"])
            .output()
            .ok()
            .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok());
        for id in listing
            .as_ref()
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|c| c["configuration"]["id"].as_str())
            .filter(|id| id.starts_with(&format!("{}-", self.instance)))
        {
            ok(&self.program, &["kill", id]);
            ok(&self.program, &["delete", "--force", id]);
        }
        ok(&self.program, &["network", "delete", &self.network]);
        // 시험이 만든 입력·쓰기 폴더와 잠금 파일도 치운다(모든 VM이 사라진 뒤다)
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn ls_ids(program: &Path, prefix: &str) -> Vec<String> {
    let output = Command::new(program)
        .args(["ls", "--all", "--format", "json"])
        .output()
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["configuration"]["id"].as_str())
        .filter(|id| id.starts_with(prefix))
        .map(str::to_owned)
        .collect()
}

/// 모델 VM 명세: 입력 폴더는 읽기 전용(/bot), scratch는 쓰기(/work).
pub fn spec(root: &Path, run: &str, script: &str, env: Vec<(String, String)>) -> ModelSpec {
    ModelSpec {
        attempt: Attempt {
            profile: ProfileId("canary".into()),
            run: RunId(run.into()),
            claim: Some(ClaimEpoch(1)),
            generation: Generation(1),
        },
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        env,
        cwd: "/bot".into(),
        readonly_mounts: vec![Mount {
            host: root.join("input"),
            guest: "/bot".into(),
        }],
        scratch: Some(Mount {
            host: root.join("work"),
            guest: "/work".into(),
        }),
        stdin: Some(b"stdin body\n".to_vec()),
        timeout: Duration::from_secs(90),
        idle_timeout: Duration::from_secs(60),
    }
}

pub async fn lines(process: &mut Box<dyn ModelProcess>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(line) = tokio::time::timeout(Duration::from_secs(90), process.next_line())
        .await
        .expect("VM 출력이 멈췄다")
    {
        out.push(line);
    }
    out
}

/// 게이트웨이(호스트)의 TCP 서비스 하나: 받은 연결마다 한 줄을 읽어 채널로 보낸다. 포트를 돌려준다.
pub fn service(
    bind: std::net::SocketAddr,
) -> (
    u16,
    std::sync::mpsc::Receiver<String>,
    std::net::TcpListener,
) {
    let listener = std::net::TcpListener::bind(bind).expect("호스트 바인드");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let accepting = listener.try_clone().unwrap();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = accepting.accept() {
            let mut buf = [0u8; 128];
            let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
            // 어댑터가 릴레이를 확인하려고 연결만 하고 끊은 것은 알리지 않는다
            if n > 0 {
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).trim().to_owned());
            }
        }
    });
    (port, rx, listener)
}

/// 이 시험의 닻 VM ID(소유 라벨이 `anchor.<인스턴스>…`이고 실제로 돌고 있는 것). 띄우는 중인 VM은 죽일 수 없으므로 돌고 있는 것만 본다.
pub fn anchor_id(program: &Path, instance: &str) -> Option<String> {
    let output = Command::new(program)
        .args(["ls", "--all", "--format", "json"])
        .output()
        .ok()?;
    let all: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    all.as_array()?.iter().find_map(|c| {
        let owner = c["configuration"]["labels"]["bot.owner"].as_str()?;
        let id = c["configuration"]["id"].as_str()?;
        (owner.starts_with(&format!("anchor.{instance}."))
            && id.starts_with(&format!("{instance}-")))
        .then(|| id.to_owned())
    })
}

/// 닻이 돌고 있을 때까지(최대 60초) 기다려 그 ID를 돌려준다.
pub async fn running_anchor(program: &Path, instance: &str) -> String {
    for _ in 0..600 {
        if let Some(id) = anchor_id(program, instance) {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("닻이 돌고 있어야 한다");
}

/// VM이 뜨는 동안에는 목록에 `stopped`로 보이는 순간이 있어 `kill`이 거절될 수 있다: 될 때까지(최대 20초) 다시 한다.
pub async fn kill_eventually(program: &Path, id: &str) -> bool {
    for _ in 0..200 {
        if ok(program, &["kill", id]) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// 가끔 실패하게 만든 `container`: 깃발 파일이 있으면 닻 VM을 띄우는 `run`만 거절하고 나머지는 진짜 CLI로 넘긴다(다시 띄우지 못하는 상황).
pub fn blockable(env: &Env) -> (PathBuf, PathBuf) {
    let flag = env.root.join("block-anchor");
    let wrapper = env.root.join("container-wrapper");
    let script = format!(
        r#"#!/bin/sh
case " $* " in
  *" run "*bot.owner=anchor.*) [ -e "{flag}" ] && exit 1 ;;
esac
exec "{real}" "$@"
"#,
        flag = flag.display(),
        real = env.program.display()
    );
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    (wrapper, flag)
}
