//! Local `claude` CLI test sandbox and full broker-front + fake-upstream + PostgreSQL host wiring.
#![allow(dead_code)]

use super::*;
use bot_broker::{
    front::{
        FrontConfig, FrontParts, ModelFront, Session, SessionTable, cli::PlannedCalls,
        observe::CheckpointSink,
    },
    model::{ModelHost, ModelLimits, ModelProxy, Protocol, Provider},
    resolve::HostPort,
    service::preflight::Preflight,
    sys::{OsEntropy, SystemClock},
    testing::{FakeSecrets, FakeUpstream, RecordingPark},
};
use bot_core::{
    ledger::CallLedger,
    port::{DelegationStore, Reply, RunStore},
    sandbox::{
        Exit, ExitFuture, LineFuture, ModelProcess, ModelSandbox, ModelSpec, Sandbox, UnitFuture,
    },
};
use bot_runtime::{
    Relays, SingleModel,
    model::{ClaudeCodeRunner, CliConfig, CliModel, RelayChannel, RelayGrant},
};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdout, Command},
};

use super::sweep::Sweep;
#[allow(unused_imports)]
pub use super::{
    sweep::{Swept, sweep_files},
    turns::*,
};

pub fn claude_bin() -> Option<PathBuf> {
    let found = std::env::var_os("BOT_CLAUDE_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PATH")?
                .to_str()?
                .split(':')
                .map(|d| PathBuf::from(d).join("claude"))
                .find(|p| p.is_file())
        });
    if found.is_none() {
        assert!(
            std::env::var("BOT_CLAUDE_CANARY").as_deref() != Ok("required"),
            "claude CLI is required (set BOT_CLAUDE_BIN)"
        );
        eprintln!("skipped: claude CLI not found");
    }
    found
}

/// Runs `claude` as a host process by mapping virtual `/bot` and `/work` paths to host directories, sweeping mounts for `RELAY_TOKEN` on exit.
pub struct LocalSandbox {
    claude: PathBuf,
    swept: Arc<std::sync::Mutex<Vec<Swept>>>,
}

impl LocalSandbox {
    pub fn new(claude: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            claude,
            swept: Arc::default(),
        })
    }

    pub fn swept(&self) -> Vec<Swept> {
        self.swept.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

fn host_path(spec: &ModelSpec, value: &str) -> String {
    let mounts = spec.readonly_mounts.iter().chain(spec.scratch.iter());
    for mount in mounts {
        if value == mount.guest {
            return mount.host.display().to_string();
        }
        if let Some(rest) = value.strip_prefix(&format!("{}/", mount.guest)) {
            return mount.host.join(rest).display().to_string();
        }
    }
    value.to_owned()
}

/// Symlinks the host-encoded cwd project directory to `-bot` so `claude --resume` finds the same project dir as inside `/bot`.
fn link_project_dir(spec: &ModelSpec, cwd: &Path) {
    let Some(work) = &spec.scratch else {
        return;
    };
    let projects = work.host.join("claude/projects");
    let _ = std::fs::create_dir_all(projects.join("-bot"));
    let encode = |p: &Path| -> String {
        p.display()
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
    sweep: Option<Sweep>,
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
            if let Some(sweep) = self.sweep.take() {
                sweep.run();
            }
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
            let mut child = command
                .spawn()
                .map_err(|_| bot_core::port::Error::Unavailable)?;
            if let (Some(body), Some(mut stdin)) = (spec.stdin.clone(), child.stdin.take()) {
                tokio::spawn(async move {
                    let _ = stdin.write_all(&body).await;
                    let _ = stdin.shutdown().await;
                });
            }
            let stdout = child
                .stdout
                .take()
                .ok_or(bot_core::port::Error::Unavailable)?;
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
            let sweep = Sweep::of(&spec, self.swept.clone());
            Ok(Box::new(LocalProcess {
                child,
                lines: BufReader::new(stdout).lines(),
                stderr,
                sweep,
            }) as Box<dyn ModelProcess>)
        })
    }

    fn reap(&self, _: &str) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

pub struct FrontRelays {
    pub sessions: Arc<SessionTable>,
    pub addr: SocketAddr,
    pub issued: std::sync::Mutex<Vec<String>>,
}

struct Revoke {
    sessions: Arc<SessionTable>,
    attempt: Attempt,
}

impl Drop for Revoke {
    fn drop(&mut self) {
        self.sessions.revoke_attempt(&self.attempt);
    }
}

impl Relays for FrontRelays {
    fn open(&self, attempt: &Attempt, agent: &AgentId, alias: &str) -> Option<RelayGrant> {
        let token = self.sessions.issue(Session {
            attempt: attempt.clone(),
            agent: agent.clone(),
            alias: alias.to_owned(),
            protocol: Protocol::Anthropic,
        })?;
        if let Ok(mut issued) = self.issued.lock() {
            issued.push(token.clone());
        }
        Some(RelayGrant::new(
            RelayChannel {
                name: "relay".into(),
                base_url: format!("http://{}", self.addr),
                token,
            },
            Revoke {
                sessions: self.sessions.clone(),
                attempt: attempt.clone(),
            },
        ))
    }
}

/// Simulated process instance owning its own store handle, session table, front, and worker.
pub struct Host {
    pub worker: Worker,
    pub front: ModelFront,
    pub store: Arc<Store>,
    pub park: Arc<RecordingPark>,
    pub relays: Arc<FrontRelays>,
}

impl Host {
    pub fn tokens(&self) -> Vec<String> {
        self.relays
            .issued
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default()
    }
}

pub async fn mcp_with(addr: SocketAddr, token: &str, body: &str) -> (u16, String) {
    use tokio::net::TcpStream;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: front\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let _ = stream.read_to_end(&mut head).await;
    let text = String::from_utf8_lossy(&head).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, rest)| rest.to_owned())
        .unwrap_or_default();
    (status, body)
}

pub async fn list_tools_with(addr: SocketAddr, token: &str) -> (u16, String) {
    mcp_with(
        addr,
        token,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
    )
    .await
}

pub async fn status_with(addr: SocketAddr, token: &str) -> u16 {
    mcp_with(addr, token, "{}").await.0
}

fn model_host() -> ModelHost {
    ModelHost {
        providers: [(
            "main".to_owned(),
            Provider {
                protocol: Protocol::Anthropic,
                endpoint: HostPort {
                    host: "api.main.example".into(),
                    port: 443,
                },
                path: "/v1/messages".into(),
                secret: SecretName::parse("model-key").unwrap(),
                header: "x-api-key".into(),
                scheme: None,
                headers: vec![],
                forward_headers: vec!["anthropic-version".into(), "anthropic-beta".into()],
                body_fields: vec![],
                models: [("assistant".to_owned(), "model-large".to_owned())].into(),
            },
        )]
        .into(),
        allow: [(ProfileId("personal".into()), ["main".to_owned()].into())].into(),
    }
}

pub async fn host(
    world: &World,
    store: Arc<Store>,
    upstream: Arc<FakeUpstream>,
    sandbox: Arc<LocalSandbox>,
    workspace: &Path,
) -> Host {
    host_judged(
        world,
        store,
        upstream,
        sandbox,
        workspace,
        world.broker.clone(),
    )
    .await
}

pub async fn host_judged(
    world: &World,
    store: Arc<Store>,
    upstream: Arc<FakeUpstream>,
    sandbox: Arc<LocalSandbox>,
    workspace: &Path,
    judge: Arc<dyn Preflight>,
) -> Host {
    host_at(
        world,
        store,
        upstream,
        "127.0.0.1:0".parse().unwrap(),
        async |_| sandbox as Arc<ModelSandbox>,
        workspace,
        judge,
    )
    .await
}

pub async fn host_at(
    world: &World,
    store: Arc<Store>,
    upstream: Arc<FakeUpstream>,
    bind: SocketAddr,
    sandbox: impl AsyncFnOnce(SocketAddr) -> Arc<ModelSandbox>,
    workspace: &Path,
    judge: Arc<dyn Preflight>,
) -> Host {
    host_tuned(
        world,
        store,
        upstream,
        bind,
        sandbox,
        workspace,
        judge,
        |_| {},
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn host_tuned(
    world: &World,
    store: Arc<Store>,
    upstream: Arc<FakeUpstream>,
    bind: SocketAddr,
    sandbox: impl AsyncFnOnce(SocketAddr) -> Arc<ModelSandbox>,
    workspace: &Path,
    judge: Arc<dyn Preflight>,
    tune: impl FnOnce(&mut WorkerConfig),
) -> Host {
    let ledger: Arc<dyn CallLedger> = store.clone();
    let model = Arc::new(ModelProxy::new(
        model_host(),
        ledger.clone(),
        Arc::new(FakeSecrets::new(&[(
            "personal",
            "model-key",
            "provider-key-for-personal",
        )])),
        upstream,
        Arc::new(SystemClock),
        ModelLimits::default(),
    ));
    // Distinct OS entropy per simulated host prevents cross-process token collisions.
    let sessions = SessionTable::new(Arc::new(OsEntropy));
    let park = Arc::new(RecordingPark::new());
    let front = ModelFront::start(
        bind,
        FrontParts {
            sessions: sessions.clone(),
            model,
            broker: world.broker.clone(),
            catalog: world.catalog.clone(),
            calls: Arc::new(
                PlannedCalls::new(ledger.clone(), Duration::from_secs(20)).with_preflight(judge),
            ),
            turns: Arc::new(CheckpointSink::new(store.clone() as Arc<dyn RunStore>)),
            ledger: ledger.clone(),
            delegation: store.clone() as Arc<dyn DelegationStore>,
            park: park.clone(),
        },
        FrontConfig::default(),
    )
    .await
    .unwrap();
    let sandbox = sandbox(front.addr()).await;
    let runner = ClaudeCodeRunner::new(
        sandbox,
        CliConfig::new(workspace)
            .with_model(
                "assistant",
                CliModel {
                    id: "model-large".into(),
                    effort: "low".into(),
                    max_output_tokens: 4096,
                    retries: 1,
                    context_window: 200_000,
                },
            )
            .with_confined_reads(),
    );
    let relays = Arc::new(FrontRelays {
        sessions,
        addr: front.addr(),
        issued: Default::default(),
    });
    let mut config = WorkerConfig {
        concurrency: 1,
        poll: Duration::from_millis(50),
        heartbeat: Duration::from_millis(300),
        heartbeat_misses: 40,
        limits: bot_runtime::model::Limits {
            total: Duration::from_secs(120),
            idle: Duration::from_secs(60),
        },
        ..WorkerConfig::default()
    };
    tune(&mut config);
    let worker = Worker::new(
        Deps {
            runs: store.clone(),
            memory: store.clone(),
            delegation: store.clone(),
            broker: world.broker.clone(),
            ledger: store.clone(),
            catalog: world.catalog.clone(),
            models: Arc::new(SingleModel(Arc::new(runner))),
            relays: relays.clone(),
            tracer: world.spans.clone(),
            gateway: None,
        },
        config,
    );
    Host {
        worker,
        front,
        store,
        park,
        relays,
    }
}
