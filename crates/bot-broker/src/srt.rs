//! Anthropic sandbox-runtime (`srt`) adapter implementing OS-level `Sandbox` isolation.
//!
//! Pins `PINNED_VERSION` with no fallback. Network egress runs through the broker's authenticated
//! `CONNECT` proxy (`parentProxy`); filesystem denies host/work roots and `/tmp/claude` (plus a
//! Linux `bwrap` tmpfs overlay); secrets pass via `BOT_CARRY_<n>` into the child so tool env names
//! cannot steer the launcher. Process-group exit is verified before `terminate` or `reap` succeeds.

mod env;
mod overlay;
mod tools;

use crate::{
    sandbox::{Handle, Pipe, Process, Sandbox, SandboxSpec},
    sys::Entropy,
    text::hex,
};
use bot_core::{
    install::ServerState,
    port::{Error, Reply},
};
use env::carry_script;
pub use env::forbidden_env_name;
#[cfg(test)]
use env::safe;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    time::timeout,
};
use tools::{Group, Tools, find_tool, private_dir, write_private};

/// Fixed 64-char hex digest prefix preventing prefix or character-substitution collisions in meta filenames.
fn owner_prefix(owner: &str) -> String {
    hex(&Sha256::digest(owner.as_bytes()))
}

fn is_owner_stem(stem: &str, prefix: &str) -> bool {
    let Some(nonce) = stem.strip_prefix(prefix) else {
        return false;
    };
    nonce.len() == 32
        && nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub const PINNED_VERSION: &str = "0.0.83";
const CARRY: &str = "BOT_CARRY_";
const HOST_ENV: [&str; 1] = ["BOT_EFFECT_URL"];
const TOMBSTONE_KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

const PROXY_VARS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// Exhaustive check for whether `srt` can satisfy a server's persistent-state requirement.
pub fn supports_state(state: ServerState) -> bool {
    match state {
        ServerState::Ephemeral => true,
        ServerState::Required => false,
    }
}

#[derive(Clone, Debug)]
pub struct ServerCommand {
    pub argv: Vec<String>,
    pub revision: String,
    pub install_dir: PathBuf,
    pub state: ServerState,
}

#[derive(Clone, Debug)]
pub struct SrtConfig {
    pub binary: PathBuf,
    pub work_root: PathBuf,
    pub servers: BTreeMap<String, ServerCommand>,
    pub deny_read: Vec<PathBuf>,
    pub path: String,
    pub terminate_wait: Duration,
    pub host_tools: Option<HostTools>,
}

#[derive(Clone, Debug)]
pub struct HostTools {
    pub kill: PathBuf,
    pub pgrep: PathBuf,
    pub ps: PathBuf,
}

struct Live {
    pgid: u32,
    work: PathBuf,
}

pub struct SrtSandbox {
    cfg: Arc<SrtConfig>,
    servers: std::sync::RwLock<BTreeMap<String, ServerCommand>>,
    tools: Tools,
    entropy: Arc<dyn Entropy>,
    live: Arc<Mutex<HashMap<String, Live>>>,
    /// Real Linux `bwrap` wrapped per call to hide `/tmp/claude` with a read-only tmpfs (`None` on macOS).
    real_bwrap: Option<PathBuf>,
}

pub fn settings(spec: &SandboxSpec, work: &Path, install: &Path, cfg: &SrtConfig) -> Value {
    let paths = |items: &[&Path]| -> Vec<String> {
        items.iter().map(|p| p.display().to_string()).collect()
    };
    let mut deny_read: Vec<&Path> = cfg.deny_read.iter().map(PathBuf::as_path).collect();
    deny_read.push(&cfg.work_root);
    // Block srt's default shared `/tmp/claude` directory from both reads and writes across calls.
    let shared_tmp = [Path::new("/tmp/claude"), Path::new("/private/tmp/claude")];
    deny_read.extend(shared_tmp);
    let mut network = json!({
        "allowedDomains": spec.egress.as_ref().map(|_| spec.egress_hosts.clone()).unwrap_or_default(),
        "deniedDomains": [],
        "strictAllowlist": true,
        "allowLocalBinding": false,
        "allowUnixSockets": [work.display().to_string()],
    });
    if let Some(endpoint) = &spec.egress {
        network["parentProxy"] = json!({"http": endpoint.url, "https": endpoint.url});
    }
    json!({
        "network": network,
        "filesystem": {
            "denyRead": paths(&deny_read),
            "allowRead": paths(&[work, install]),
            "allowWrite": paths(&[work]),
            "denyWrite": paths(&shared_tmp),
        },
    })
}

impl SrtSandbox {
    pub async fn open(cfg: SrtConfig, entropy: Arc<dyn Entropy>) -> Result<Self, Error> {
        let tools = match cfg.host_tools.clone() {
            Some(tools) => tools,
            None => Tools {
                kill: find_tool(&["/bin/kill", "/usr/bin/kill"]).ok_or(Error::Unavailable)?,
                pgrep: find_tool(&["/usr/bin/pgrep", "/bin/pgrep"]).ok_or(Error::Unavailable)?,
                ps: find_tool(&["/bin/ps", "/usr/bin/ps"]).ok_or(Error::Unavailable)?,
            },
        };
        let output = timeout(
            Duration::from_secs(15),
            Command::new(&cfg.binary)
                .arg("--version")
                .env_clear()
                .env("PATH", &cfg.path)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(|_| Error::Unavailable)?;
        let text = String::from_utf8_lossy(&output.stdout);
        let seen = text
            .split_whitespace()
            .last()
            .map(|t| t.trim_start_matches('v'));
        if !output.status.success() || seen != Some(PINNED_VERSION) {
            return Err(Error::Unavailable);
        }
        private_dir(&cfg.work_root).map_err(|_| Error::Unavailable)?;
        let real_bwrap = overlay::locate(std::env::consts::OS, &cfg.path);
        let servers = std::sync::RwLock::new(cfg.servers.clone());
        let sandbox = Self {
            cfg: Arc::new(cfg),
            servers,
            tools,
            entropy,
            live: Arc::default(),
            real_bwrap,
        };
        private_dir(&sandbox.meta_dir()).map_err(|_| Error::Unavailable)?;
        sandbox.purge_old_tombstones();
        Ok(sandbox)
    }

    fn meta_dir(&self) -> PathBuf {
        self.cfg.work_root.join(".srt")
    }

    fn purge_old_tombstones(&self) {
        let Ok(entries) = std::fs::read_dir(self.meta_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let old = path.extension().and_then(|e| e.to_str()) == Some("done")
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| SystemTime::now().duration_since(m).ok())
                    .is_some_and(|age| age > TOMBSTONE_KEEP);
            if old {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Removes call artifacts and writes a `.done` tombstone before removing `.pid` so repeat `reap` succeeds.
    fn settle(meta_dir: &Path, name: &str, work: &Path) {
        let _ = std::fs::remove_dir_all(work);
        let _ = std::fs::remove_file(meta_dir.join(format!("{name}.json")));
        let _ = std::fs::remove_file(meta_dir.join(format!("{name}.bwrap")));
        let _ = write_private(&meta_dir.join(format!("{name}.done")), "done\n");
        let _ = std::fs::remove_file(meta_dir.join(format!("{name}.pid")));
    }

    pub fn update_servers(&self, next: BTreeMap<String, ServerCommand>) {
        *self.servers.write().unwrap_or_else(|e| e.into_inner()) = next;
    }

    fn admitted(&self, server: &str, image: &str) -> Result<ServerCommand, Error> {
        let servers = self.servers.read().unwrap_or_else(|e| e.into_inner());
        let found = servers
            .get(server)
            .filter(|s| s.revision == image)
            .ok_or(Error::Denied)?;
        if !supports_state(found.state) {
            return Err(Error::Denied);
        }
        Ok(found.clone())
    }

    async fn start(&self, spec: SandboxSpec) -> Result<Process, Error> {
        let server = self.admitted(&spec.server, &spec.image)?;
        if spec.effects {
            return Err(Error::Unavailable);
        }
        // Proxy env vars are managed by srt itself; reject forbidden names in remaining declared env.
        let secrets: Vec<&(String, String)> = spec
            .env
            .iter()
            .filter(|(k, _)| !PROXY_VARS.contains(&k.as_str()))
            .collect();
        if secrets
            .iter()
            .any(|(k, v)| forbidden_env_name(k) || v.contains('\0'))
        {
            return Err(Error::Denied);
        }
        if spec
            .host_env
            .iter()
            .any(|(k, v)| !HOST_ENV.contains(&k.as_str()) || v.contains('\0'))
        {
            return Err(Error::Denied);
        }
        let mut random = [0u8; 16];
        if !self.entropy.fill(&mut random) {
            return Err(Error::Unavailable);
        }
        let name = format!("{}-{}", owner_prefix(&spec.owner), hex(&random));
        let (work, meta_dir) = (self.cfg.work_root.join(&name), self.meta_dir());
        let (settings_path, meta) = (
            meta_dir.join(format!("{name}.json")),
            meta_dir.join(format!("{name}.pid")),
        );
        private_dir(&meta_dir)
            .and_then(|_| private_dir(&work))
            .map_err(|_| Error::Unavailable)?;
        let mut value = settings(&spec, &work, &server.install_dir, &self.cfg);
        if let Some(real) = &self.real_bwrap {
            let launcher = meta_dir.join(format!("{name}.bwrap"));
            overlay::write_launcher(&launcher, real).map_err(|_| Error::Unavailable)?;
            let launcher = std::path::absolute(launcher).map_err(|_| Error::Unavailable)?;
            value["bwrapPath"] = json!(launcher.display().to_string());
        }
        write_private(&settings_path, &value.to_string()).map_err(|_| Error::Unavailable)?;
        // Record spawn intent first so crashing before writing the PGID fails `reap` closed.
        write_private(&meta, &format!("pending\n{}\n", work.display()))
            .map_err(|_| Error::Unavailable)?;
        let mut command = Command::new(&self.cfg.binary);
        command
            .arg("--settings")
            .arg(&settings_path)
            .arg("--")
            .env_clear()
            .env("PATH", &self.cfg.path)
            // Redirect srt's child TMPDIR away from the shared `/tmp/claude` path.
            .env("CLAUDE_CODE_TMPDIR", &work)
            .current_dir(&work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        if secrets.is_empty() && spec.host_env.is_empty() {
            command.args(&server.argv);
        } else {
            // Pass values through fixed `BOT_CARRY_<n>` slots and rename inside the isolated child.
            let all: Vec<&(String, String)> = secrets
                .iter()
                .copied()
                .chain(spec.host_env.iter())
                .collect();
            let names: Vec<&str> = all.iter().map(|(k, _)| k.as_str()).collect();
            command
                .args(["/bin/sh", "-c", &carry_script(&names), "sh"])
                .args(&server.argv);
            for (i, (_, value)) in all.iter().enumerate() {
                command.env(format!("{CARRY}{i}"), value);
            }
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                Self::settle(&meta_dir, &name, &work);
                return Err(Error::Unavailable);
            }
        };
        let (Some(pgid), Some(stdin), Some(stdout), Some(mut stderr)) = (
            child.id(),
            child.stdin.take(),
            child.stdout.take(),
            child.stderr.take(),
        ) else {
            let _ = child.start_kill();
            return Err(Error::Unavailable);
        };
        // Drain stderr without buffering so noisy tools cannot deadlock on a full pipe.
        tokio::spawn(async move {
            let mut sink = [0u8; 4096];
            while matches!(stderr.read(&mut sink).await, Ok(n) if n > 0) {}
        });
        write_private(&meta, &format!("{pgid}\n{}\n", work.display()))
            .map_err(|_| Error::Unavailable)?;
        self.live.lock().unwrap_or_else(|e| e.into_inner()).insert(
            spec.owner.clone(),
            Live {
                pgid,
                work: work.clone(),
            },
        );
        Ok(Process {
            stdio: Pipe {
                read: Box::new(stdout),
                write: Box::new(stdin),
            },
            effects: None,
            handle: Box::new(SrtHandle {
                child,
                pgid,
                owner: spec.owner,
                name,
                work,
                meta_dir,
                tools: self.tools.clone(),
                live: Arc::clone(&self.live),
                wait: self.cfg.terminate_wait,
            }),
        })
    }
}

struct SrtHandle {
    child: Child,
    pgid: u32,
    owner: String,
    name: String,
    work: PathBuf,
    meta_dir: PathBuf,
    tools: Tools,
    live: Arc<Mutex<HashMap<String, Live>>>,
    wait: Duration,
}

impl Handle for SrtHandle {
    fn terminate(self: Box<Self>) -> Reply<'static, ()> {
        Box::pin(async move {
            let mut me = *self;
            me.tools.kill_group(me.pgid).await;
            let _ = timeout(me.wait, me.child.wait()).await;
            if me.tools.gone_within(me.pgid, me.wait).await {
                me.live
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&me.owner);
                SrtSandbox::settle(&me.meta_dir, &me.name, &me.work);
                Ok(())
            } else {
                Err(Error::Unavailable)
            }
        })
    }
}

impl Sandbox for SrtSandbox {
    fn spawn(&self, spec: SandboxSpec) -> Reply<'_, Process> {
        Box::pin(self.start(spec))
    }

    fn effects_via_proxy(&self) -> bool {
        true
    }

    fn admit(&self, server: &str, image: &str) -> Result<(), Error> {
        self.admitted(server, image).map(|_| ())
    }

    /// Proves every process group created for `owner` is gone; fails closed (`Unavailable`) if
    /// metadata is unreadable, absent, incomplete (`pending`), or the group cannot be verified gone.
    fn reap(&self, owner: &str) -> Reply<'_, ()> {
        let owner = owner.to_owned();
        Box::pin(async move {
            let wait = self.cfg.terminate_wait;
            let live = self
                .live
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&owner);
            if let Some(live) = live {
                self.tools.kill_group(live.pgid).await;
                if !self.tools.gone_within(live.pgid, wait).await {
                    self.live
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(owner.clone(), live);
                    return Err(Error::Unavailable);
                }
                let _ = std::fs::remove_dir_all(&live.work);
            }
            let prefix = format!("{}-", owner_prefix(&owner));
            let entries = match std::fs::read_dir(self.meta_dir()) {
                Ok(entries) => entries,
                Err(_) => return Err(Error::Unavailable),
            };
            let (mut proved, mut running) = (false, Vec::new());
            for entry in entries {
                let path = entry.map_err(|_| Error::Unavailable)?.path();
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_owned();
                if !is_owner_stem(&stem, &prefix) {
                    continue;
                }
                match path.extension().and_then(|e| e.to_str()) {
                    Some("done") => proved = true,
                    Some("pid") => running.push((stem, path)),
                    _ => {}
                }
            }
            for (stem, path) in running {
                let text = std::fs::read_to_string(&path).map_err(|_| Error::Unavailable)?;
                let mut lines = text.lines();
                let pgid: u32 = lines
                    .next()
                    .and_then(|l| l.parse().ok())
                    .ok_or(Error::Unavailable)?;
                let work = PathBuf::from(lines.next().ok_or(Error::Unavailable)?);
                match self.tools.probe(pgid).await {
                    Group::Unknown => return Err(Error::Unavailable),
                    Group::Alive => {
                        // Guard against PID reuse by verifying the group leader's command line names our stem.
                        let leader = self
                            .tools
                            .leader_command(pgid)
                            .await
                            .ok_or(Error::Unavailable)?;
                        if !leader.contains(&stem) {
                            return Err(Error::Unavailable);
                        }
                        self.tools.kill_group(pgid).await;
                        if !self.tools.gone_within(pgid, wait).await {
                            return Err(Error::Unavailable);
                        }
                    }
                    Group::Gone => {}
                }
                Self::settle(&self.meta_dir(), &stem, &work);
                proved = true;
            }
            if proved {
                Ok(())
            } else {
                Err(Error::Unavailable)
            }
        })
    }
}

#[cfg(test)]
mod tests;
