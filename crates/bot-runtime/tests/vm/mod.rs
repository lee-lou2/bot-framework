//! Shared Apple `container` canary harness: environment probing, per-test isolated names/networks, cleanup, and VM specs.
//! Only touches `botvm-c<pid>*` containers and `botvm-canary-<pid>*` networks created by this test.
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

/// Serializes real VM canary tests across threads and processes via a host lock file to avoid vmnet interference.
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
        .expect("VM canary lock file");
    for _ in 0..4500 {
        if rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok() {
            return file;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!("warning: another VM test held the lock too long; proceeding unlocked");
    file
}

pub fn available() -> Option<PathBuf> {
    let required = std::env::var("BOT_CONTAINER_CANARY").as_deref() == Ok("required");
    let program = container_bin()
        .filter(|p| ok(p, &["system", "status"]) && ok(p, &["image", "inspect", IMAGE]));
    if program.is_none() {
        assert!(
            !required,
            "BOT_CONTAINER_CANARY=required: container CLI, service, and local {IMAGE} image are required"
        );
        eprintln!("skipped: container CLI, its service or the local {IMAGE} image is unavailable");
    }
    program
}

/// Resolves `BOT_MODEL_IMAGE=<name@sha256:...>` (built locally via `images/model/build.sh`); skips unless `BOT_CONTAINER_CANARY=required`.
pub fn model_image(program: &Path) -> Option<String> {
    let required = std::env::var("BOT_CONTAINER_CANARY").as_deref() == Ok("required");
    let image = std::env::var("BOT_MODEL_IMAGE")
        .ok()
        .filter(|image| image.contains("@sha256:") && ok(program, &["image", "inspect", image]));
    if image.is_none() {
        assert!(
            !required,
            "BOT_CONTAINER_CANARY=required: local BOT_MODEL_IMAGE=<name@sha256:...> image is required (images/model/build.sh)"
        );
        eprintln!("skipped: BOT_MODEL_IMAGE (a local name@sha256:... model image) is not set");
    }
    image
}

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
        std::fs::write(root.join("input/in.txt"), "read-only input").unwrap();
        Self {
            program,
            network: format!("botvm-canary-{pid}-{tag}"),
            instance: format!("botvm-c{pid}{tag}"),
            root,
        }
    }

    /// Picks a subnet in `192.168.200..249.0/24` outside `container`'s default auto-allocation range.
    pub fn subnet(&self) -> String {
        let seed = self.network.bytes().fold(std::process::id(), |h, b| {
            h.wrapping_mul(31).wrapping_add(u32::from(b))
        });
        format!("192.168.{}.0/24", 200 + seed % 50)
    }

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

    pub fn ids(&self) -> Vec<String> {
        ls_ids(&self.program, &format!("{}-", self.instance))
    }

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
        .expect("VM output stalled")
    {
        out.push(line);
    }
    out
}

pub fn service(
    bind: std::net::SocketAddr,
) -> (
    u16,
    std::sync::mpsc::Receiver<String>,
    std::net::TcpListener,
) {
    let listener = std::net::TcpListener::bind(bind).expect("host bind");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let accepting = listener.try_clone().unwrap();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = accepting.accept() {
            let mut buf = [0u8; 128];
            let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
            // Ignore zero-byte readiness probes from the adapter.
            if n > 0 {
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).trim().to_owned());
            }
        }
    });
    (port, rx, listener)
}

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

pub async fn running_anchor(program: &Path, instance: &str) -> String {
    for _ in 0..600 {
        if let Some(id) = anchor_id(program, instance) {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("anchor must be running");
}

/// Retries `kill` because a newly spawned VM can briefly appear `stopped` before accepting signals.
pub async fn kill_eventually(program: &Path, id: &str) -> bool {
    for _ in 0..200 {
        if ok(program, &["kill", id]) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Wraps `container` so anchor `run` commands fail while `flag` exists.
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
