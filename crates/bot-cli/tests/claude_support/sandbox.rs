//! 합성 샌드박스: 가상 경로(`/bot`·`/work`)를 호스트 폴더로 옮겨 모델 CLI를 호스트 프로세스로 띄운다(Apple VM 대신, 시험 전용). 환경은 상속하지 않는다.
//! 문(`Gate`)을 닫아 두면 CLI를 띄우기 전에 멈춘다 — 시도는 집었지만 모델 요청은 아직 안 나간 순간을 만든다.
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

/// 모델 CLI를 띄우기 전에 서는 문. 닫으면(`hold`) 열 때(`release`)까지 `spawn`이 기다린다.
pub struct Gate(watch::Sender<bool>);

impl Gate {
    /// 처음에는 열려 있다.
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

/// 값 앞부분이 가상 마운트 경로면 호스트 경로로 바꾼다.
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

/// CLI는 작업 폴더 경로로 프로젝트 폴더 이름을 정한다. 가상 `/bot`이 호스트의 다른 경로가 되므로, 그 이름의 폴더를 `-bot`으로 잇는다.
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
            // 문이 닫혀 있으면 열릴 때까지 CLI를 띄우지 않는다
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
