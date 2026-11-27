//! Test-only synthetic sandbox that maps guest mounts (`/bot`, `/work`) to host directories and spawns the model CLI locally without inheriting env.
#![allow(dead_code)]

use bot_core::{
    port::{Error, Reply},
    sandbox::{Exit, ExitFuture, LineFuture, ModelProcess, ModelSpec, Sandbox, UnitFuture},
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdout, Command},
    sync::watch,
};

/// Pre-spawn gate: when held, `spawn` waits until released so tests can pause between claim and model invocation.
pub struct Gate(watch::Sender<bool>);

impl Gate {
    pub fn new() -> Self {
        Self(watch::channel(true).0)
    }

    pub fn hold(&self) {
        let _ = self.0.send(false);
    }

    pub fn release(&self) {
        let _ = self.0.send(true);
    }

    fn watch(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

pub struct LocalSandbox {
    claude: PathBuf,
    gate: watch::Receiver<bool>,
}

impl LocalSandbox {
    pub fn new(claude: PathBuf, gate: &Gate) -> Arc<Self> {
        Arc::new(Self {
            claude,
            gate: gate.watch(),
        })
    }
}

fn host_path(spec: &ModelSpec, value: &str) -> String {
    for mount in spec.readonly_mounts.iter().chain(spec.scratch.iter()) {
        if value == mount.guest {
            return mount.host.display().to_string();
        }
        if let Some(rest) = value.strip_prefix(&format!("{}/", mount.guest)) {
            return mount.host.join(rest).display().to_string();
        }
    }
    value.to_owned()
}

/// Symlinks the host-encoded cwd project directory to `-bot` so CLI session lookups match the guest `/bot` path.
fn link_project_dir(spec: &ModelSpec, cwd: &Path) {
    let Some(work) = &spec.scratch else {
        return;
    };
    let projects = work.host.join("claude/projects");
    let _ = std::fs::create_dir_all(projects.join("-bot"));
    let encode = |path: &Path| -> String {
        path.display()
            .to_string()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    };
    let mut names = vec![encode(cwd)];
    if let Ok(real) = cwd.canonicalize() {
        names.push(encode(&real));
    }
    for name in names {
        let link = projects.join(&name);
        if name != "-bot" && !link.exists() {
            let _ = std::os::unix::fs::symlink("-bot", link);
        }
    }
}

struct LocalProcess {
    child: Child,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
    stderr: Arc<std::sync::Mutex<String>>,
}

impl ModelProcess for LocalProcess {
    fn next_line(&mut self) -> LineFuture<'_> {
        Box::pin(async move { self.lines.next_line().await.ok().flatten() })
    }
    fn kill(&mut self) -> UnitFuture<'_> {
        Box::pin(async move {
            let _ = self.child.kill().await;
        })
    }
    fn wait(&mut self) -> ExitFuture<'_> {
        Box::pin(async move {
            let status = self.child.wait().await.ok();
            let tail = self.stderr.lock().map(|s| s.clone()).unwrap_or_default();
            Exit {
                code: status.and_then(|s| s.code()),
                stderr_tail: tail,
            }
        })
    }
}

impl Sandbox<ModelSpec> for LocalSandbox {
    type Process = Box<dyn ModelProcess>;

    fn spawn(&self, spec: ModelSpec) -> Reply<'_, Self::Process> {
        Box::pin(async move {
            let mut gate = self.gate.clone();
            let _ = gate.wait_for(|open| *open).await;
            let cwd = PathBuf::from(host_path(&spec, &spec.cwd));
            link_project_dir(&spec, &cwd);
            if let Some(work) = &spec.scratch {
                let _ = std::fs::create_dir_all(work.host.join("home"));
            }
            let mut command = Command::new(&self.claude);
            command
                .args(spec.args.iter().map(|a| host_path(&spec, a)))
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("NO_PROXY", "127.0.0.1,localhost")
                .env("no_proxy", "127.0.0.1,localhost")
                .current_dir(&cwd)
                .stdin(if spec.stdin.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            for (key, value) in &spec.env {
                command.env(key, host_path(&spec, value));
            }
            let mut child = command.spawn().map_err(|_| Error::Unavailable)?;
            if let (Some(body), Some(mut stdin)) = (spec.stdin.clone(), child.stdin.take()) {
                tokio::spawn(async move {
                    let _ = stdin.write_all(&body).await;
                    let _ = stdin.shutdown().await;
                });
            }
            let stdout = child.stdout.take().ok_or(Error::Unavailable)?;
            let stderr = Arc::new(std::sync::Mutex::new(String::new()));
            if let Some(mut pipe) = child.stderr.take() {
                let sink = stderr.clone();
                tokio::spawn(async move {
                    let mut chunk = [0u8; 4096];
                    while let Ok(n) = pipe.read(&mut chunk).await {
                        if n == 0 {
                            break;
                        }
                        if let Ok(mut tail) = sink.lock() {
                            tail.push_str(&String::from_utf8_lossy(&chunk[..n]));
                            let keep = tail.len().saturating_sub(2048);
                            if keep > 0 {
                                let cut = (keep..tail.len())
                                    .find(|i| tail.is_char_boundary(*i))
                                    .unwrap_or(0);
                                *tail = tail[cut..].to_owned();
                            }
                        }
                    }
                });
            }
            Ok(Box::new(LocalProcess {
                child,
                lines: BufReader::new(stdout).lines(),
                stderr,
            }) as Box<dyn ModelProcess>)
        })
    }

    fn reap(&self, _: &str) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}
