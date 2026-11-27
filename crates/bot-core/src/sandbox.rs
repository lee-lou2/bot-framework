//! Model VM sandbox port (`ContainerSandbox` in `bot-runtime`), distinct from tool `srt` sandboxes.
use crate::{id::Attempt, port::Reply};
use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};

/// Security boundary for isolated model execution (`docs/security.md`):
/// - Do not inherit host env; expose only `env` (zero secrets, zero network, only the claim's broker relay socket).
/// - Mount `readonly_mounts` read-only and isolate `scratch` to this call; fail closed if OS isolation cannot be established.
/// - `kill` terminates descendant processes; `timeout` provides a final self-termination backstop.
pub trait Sandbox<Spec>: Send + Sync {
    type Process: Send;

    /// Must be cancel-safe: dropping the future cleans up started resources, or returns `Process` immediately so the caller can `kill` it.
    fn spawn(&self, spec: Spec) -> Reply<'_, Self::Process>;
    /// Succeeds only after all descendant processes owned by `owner` have exited.
    fn reap(&self, owner: &str) -> Reply<'_, ()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mount {
    pub host: PathBuf,
    pub guest: String,
}

/// Host-assembled model CLI request restricted to the claim's broker relay, read-only inputs, and `scratch`.
#[derive(Clone, Eq, PartialEq)]
pub struct ModelSpec {
    pub attempt: Attempt,
    pub program: String,
    pub args: Vec<String>,
    /// Secret-free CLI environment; host environment is never inherited.
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub readonly_mounts: Vec<Mount>,
    pub scratch: Option<Mount>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    pub idle_timeout: Duration,
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct Exit {
    pub code: Option<i32>,
    /// Length-capped diagnostics for internal host error classification only.
    pub stderr_tail: String,
}

pub type LineFuture<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;
pub type UnitFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
pub type ExitFuture<'a> = Pin<Box<dyn Future<Output = Exit> + Send + 'a>>;

pub trait ModelProcess: Send {
    fn next_line(&mut self) -> LineFuture<'_>;
    fn kill(&mut self) -> UnitFuture<'_>;
    fn wait(&mut self) -> ExitFuture<'_>;
}

/// Worker-injected model sandbox port; never receives tool specs or external network settings.
pub type ModelSandbox = dyn Sandbox<ModelSpec, Process = Box<dyn ModelProcess>>;
