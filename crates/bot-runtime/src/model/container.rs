//! Apple `container` VM sandbox adapter running model CLIs on a host-only (`--internal`) network.
//!
//! Startup is typestate-enforced: `ContainerSandbox::start` locks instance ownership, validates the
//! host-only network, pins the local image by digest, reaps orphans, and launches the anchor VM to
//! keep the gateway IP alive before `Anchored::relay` verifies the broker relay and yields `Sandbox`.
//! Every `spawn` rechecks instance lock, anchor health, gateway IP, network mode, and pinned image.

mod anchor;
mod args;
mod lock;
mod process;
#[cfg(test)]
mod tests;

pub use anchor::Health;
pub use args::{ContainerConfig, Entry, Refused, owner_of};
pub use process::ContainerProcess;

use anchor::Anchor;
use args::{
    INCARNATION_LABEL, INSTANCE_LABEL, MANAGED_LABEL, OWNER_LABEL, Plan, Role, is_ours,
    label_value, pin,
};
use bot_core::{
    id::Attempt,
    port::{Error, Reply},
    sandbox::{ModelProcess, ModelSpec, Sandbox},
};
use lock::{Identity, lock_instance};
use serde_json::Value;
use std::{
    net::{Ipv4Addr, SocketAddr},
    process::Stdio,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::watch,
    time::{Instant, timeout},
};

const COMMAND_WITHIN: Duration = Duration::from_secs(30);
const RELAY_UP_WITHIN: Duration = Duration::from_secs(2);
const REAP_ROUNDS: usize = 12;
/// Aligned with `WorkerConfig::cleanup` (10s) so reap subcommands stay within the shutdown budget.
const REAP_WITHIN: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartError(pub &'static str);

struct Checked {
    gateway: String,
    image: String,
}

fn host_has(address: Ipv4Addr) -> bool {
    std::net::TcpListener::bind((address, 0)).is_ok()
}

fn relay_listening(addr: SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&addr, RELAY_UP_WITHIN).is_ok()
}

#[derive(Clone)]
struct Hooks {
    host_has: Arc<dyn Fn(Ipv4Addr) -> bool + Send + Sync>,
    relay_up: Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>,
    ready_within: Duration,
    reap_pause: Duration,
    reap_within: Duration,
    address_poll: Duration,
    /// Bounds `ready()` inside `spawn`, which runs outside the worker's cancel select.
    ready_check: Duration,
}

impl Hooks {
    fn real() -> Self {
        Self {
            host_has: Arc::new(host_has),
            relay_up: Arc::new(relay_listening),
            ready_within: Duration::from_secs(60),
            reap_pause: Duration::from_millis(500),
            reap_within: REAP_WITHIN,
            address_poll: Duration::from_secs(1),
            ready_check: Duration::from_secs(5),
        }
    }
}

fn new_incarnation() -> String {
    bot_core::random::random_hex(8)
}

enum Filter<'a> {
    Orphans,
    Owner(&'a str),
    Mine,
}

pub(crate) struct Inner {
    config: ContainerConfig,
    incarnation: String,
    serial: AtomicU64,
    pinned: OnceLock<String>,
    address: OnceLock<Ipv4Addr>,
    relay: OnceLock<SocketAddr>,
    gate: OnceLock<watch::Receiver<Health>>,
    lock: OnceLock<Identity>,
    hooks: Hooks,
}

impl Inner {
    fn new(config: ContainerConfig, hooks: Hooks) -> Self {
        Self {
            config,
            incarnation: new_incarnation(),
            serial: AtomicU64::new(0),
            pinned: OnceLock::new(),
            address: OnceLock::new(),
            relay: OnceLock::new(),
            gate: OnceLock::new(),
            lock: OnceLock::new(),
            hooks,
        }
    }

    /// Runs the `container` CLI with a minimal scrubbed environment rather than inheriting host secrets.
    fn client(&self) -> Command {
        let mut command = Command::new(&self.config.program);
        command.env_clear();
        for key in ["PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "LANG"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command.stdin(Stdio::null()).kill_on_drop(true);
        command
    }

    async fn json(&self, args: &[&str], within: Duration) -> Option<Value> {
        let output = timeout(
            within,
            self.client()
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        output
            .status
            .success()
            .then(|| serde_json::from_slice(&output.stdout).ok())
            .flatten()
    }

    async fn ok(&self, args: &[&str], within: Duration) -> bool {
        timeout(
            within,
            self.client()
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .is_some_and(|status| status.success())
    }

    async fn network(&self) -> Option<Value> {
        let found = self
            .json(
                &["network", "inspect", &self.config.network],
                COMMAND_WITHIN,
            )
            .await?;
        found.as_array()?.first().cloned()
    }

    async fn preflight(&self) -> Result<Checked, StartError> {
        let mut found = self.network().await;
        if found.is_none() {
            let label = format!("{MANAGED_LABEL}=1");
            let mut create = vec!["network", "create", "--internal", "--label", &label];
            if let Some(subnet) = &self.config.subnet {
                create.extend(["--subnet", subnet]);
            }
            create.push(&self.config.network);
            let created = self.ok(&create, COMMAND_WITHIN).await;
            if !created {
                return Err(StartError("network_create_failed"));
            }
            found = self.network().await;
        }
        let first = found.ok_or(StartError("network_unreadable"))?;
        if first["configuration"]["mode"] != "hostOnly" {
            return Err(StartError("network_not_internal"));
        }
        let gateway = first["status"]["ipv4Gateway"]
            .as_str()
            .filter(|g| !g.is_empty() && g.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
            .ok_or(StartError("network_no_gateway"))?
            .to_owned();
        let inspected = self
            .json(&["image", "inspect", &self.config.image], COMMAND_WITHIN)
            .await
            .ok_or(StartError("image_missing"))?;
        let item = inspected
            .as_array()
            .and_then(|a| a.first())
            .unwrap_or(&inspected);
        let digest = item["configuration"]["descriptor"]["digest"]
            .as_str()
            .ok_or(StartError("image_unpinnable"))?;
        let image = pin(&self.config.image, digest).ok_or(StartError("image_unpinnable"))?;
        Ok(Checked { gateway, image })
    }

    async fn ready(&self) -> bool {
        let (Some(gate), Some(address), Some(image), Some(lock)) = (
            self.gate.get(),
            self.address.get(),
            self.pinned.get(),
            self.lock.get(),
        ) else {
            return false;
        };
        // Fail closed if the instance lock file was replaced or unlinked.
        if !lock.holds() {
            return false;
        }
        if !matches!(*gate.borrow(), Health::Up { .. }) {
            return false;
        }
        if !(self.hooks.host_has)(*address) {
            return false;
        }
        let inspect = ["image", "inspect", image];
        let (network, present) = tokio::join!(self.network(), self.ok(&inspect, COMMAND_WITHIN));
        present
            && network.is_some_and(|n| {
                n["configuration"]["mode"] == "hostOnly"
                    && n["status"]["ipv4Gateway"].as_str() == Some(address.to_string().as_str())
            })
    }

    /// Lists only VMs proven to belong to this instance (managed + instance labels + exact name shape).
    async fn listed(&self, filter: &Filter<'_>, within: Duration) -> Option<Vec<String>> {
        let all = self
            .json(&["ls", "--all", "--format", "json"], within)
            .await?;
        let instance = label_value(&self.config.instance);
        Some(
            all.as_array()?
                .iter()
                .filter(|c| {
                    let labels = &c["configuration"]["labels"];
                    let incarnation = labels[INCARNATION_LABEL].as_str();
                    labels[MANAGED_LABEL] == "1"
                        && labels[INSTANCE_LABEL].as_str() == Some(instance.as_str())
                        && match filter {
                            Filter::Orphans => incarnation != Some(self.incarnation.as_str()),
                            Filter::Owner(owner) => {
                                labels[OWNER_LABEL].as_str() == Some(label_value(owner).as_str())
                            }
                            Filter::Mine => incarnation == Some(self.incarnation.as_str()),
                        }
                })
                .filter_map(|c| {
                    c["configuration"]["id"]
                        .as_str()
                        .or_else(|| c["id"].as_str())
                        .filter(|id| is_ours(&self.config.prefix, id))
                        .map(str::to_owned)
                })
                .collect(),
        )
    }

    async fn reap_matching(&self, filter: Filter<'_>) -> Result<usize, Error> {
        let deadline = Instant::now() + self.hooks.reap_within;
        let within = process::budget(deadline, COMMAND_WITHIN).ok_or(Error::Unavailable)?;
        let found = self
            .listed(&filter, within)
            .await
            .ok_or(Error::Unavailable)?;
        if found.is_empty() {
            return Ok(0);
        }
        let mut left = found.clone();
        for round in 0..REAP_ROUNDS {
            if round > 0 {
                let pause =
                    process::budget(deadline, self.hooks.reap_pause).ok_or(Error::Unavailable)?;
                tokio::time::sleep(pause).await;
            }
            for id in &left {
                let within = process::budget(deadline, COMMAND_WITHIN).ok_or(Error::Unavailable)?;
                let _ = self.ok(&["kill", id], within).await;
                let within = process::budget(deadline, COMMAND_WITHIN).ok_or(Error::Unavailable)?;
                let _ = self.ok(&["delete", "--force", id], within).await;
            }
            // Confirm disappearance by re-listing; an unreadable list fails closed.
            let within = process::budget(deadline, COMMAND_WITHIN).ok_or(Error::Unavailable)?;
            left = self
                .listed(&filter, within)
                .await
                .ok_or(Error::Unavailable)?
                .into_iter()
                .filter(|id| found.contains(id))
                .collect();
            if left.is_empty() {
                return Ok(found.len());
            }
        }
        Err(Error::Unavailable)
    }

    async fn run_vm(
        &self,
        role: Role,
        spec: Option<&ModelSpec>,
    ) -> Result<ContainerProcess, Error> {
        let image = self.pinned.get().ok_or(Error::Unavailable)?;
        let plan = Plan {
            image,
            incarnation: &self.incarnation,
            serial: self.serial.fetch_add(1, Ordering::Relaxed),
            relay: self.relay.get().copied(),
        };
        let (command, stdin) = match (role, spec) {
            (Role::Model, Some(spec)) => (
                args::build(&self.config, spec, &plan).map_err(|_| Error::Invalid)?,
                spec.stdin.clone(),
            ),
            (Role::Anchor, _) => (
                args::build_anchor(&self.config, &plan).map_err(|_| Error::Invalid)?,
                None,
            ),
            (Role::Model, None) => return Err(Error::Invalid),
        };
        let mut client = self.client();
        client
            .args(&command.args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &command.secret_env {
            client.env(key, value);
        }
        let mut child = client.spawn().map_err(|_| Error::Unavailable)?;
        if let (Some(body), Some(mut pipe)) = (stdin, child.stdin.take()) {
            tokio::spawn(async move {
                let _ = pipe.write_all(&body).await;
                let _ = pipe.shutdown().await;
            });
        }
        ContainerProcess::new(child, self.config.program.clone(), command.name)
            .ok_or(Error::Unavailable)
    }
}

/// Preflight-verified and anchored handle awaiting the host to bind and confirm the relay port.
pub struct Anchored {
    inner: Arc<Inner>,
    anchor: Anchor,
    address: Ipv4Addr,
}

impl Anchored {
    /// Verified host-only gateway IP; the broker front must bind strictly to this address.
    pub fn address(&self) -> Ipv4Addr {
        self.address
    }

    pub fn bind_addr(&self, port: u16) -> SocketAddr {
        SocketAddr::new(self.address.into(), port)
    }

    pub fn health(&self) -> watch::Receiver<Health> {
        self.anchor.health()
    }

    /// Confirms the broker relay is reachable on `port` and locks it as the VM's sole relay target.
    pub async fn relay(self, port: u16) -> Result<ContainerSandbox, StartError> {
        let relay = SocketAddr::new(self.address.into(), port);
        let probe = self.inner.hooks.relay_up.clone();
        let up = port != 0
            && tokio::task::spawn_blocking(move || probe(relay))
                .await
                .unwrap_or(false);
        if !up || self.inner.relay.set(relay).is_err() {
            self.anchor.shutdown().await;
            return Err(StartError(if port == 0 {
                "relay_port"
            } else {
                "relay_unreachable"
            }));
        }
        Ok(ContainerSandbox {
            inner: self.inner,
            anchor: Mutex::new(Some(self.anchor)),
        })
    }

    pub async fn abort(self) {
        self.anchor.shutdown().await;
    }
}

pub struct ContainerSandbox {
    inner: Arc<Inner>,
    anchor: Mutex<Option<Anchor>>,
}

impl ContainerSandbox {
    pub async fn start(config: ContainerConfig) -> Result<Anchored, StartError> {
        Self::start_with(config, Hooks::real()).await
    }

    async fn start_with(config: ContainerConfig, hooks: Hooks) -> Result<Anchored, StartError> {
        args::valid(&config).map_err(|_| StartError("config"))?;
        if config.entry.is_none() && !config.unfirewalled {
            return Err(StartError("firewall_required"));
        }
        let lock = lock_instance(&config).await?;
        let inner = Arc::new(Inner::new(config, hooks));
        let _ = inner.lock.set(lock.identity());
        let checked = inner.preflight().await?;
        let address = anchor::gateway_ip(&checked.gateway)?;
        let _ = inner.pinned.set(checked.image);
        let _ = inner.address.set(address);
        inner
            .reap_matching(Filter::Orphans)
            .await
            .map_err(|_| StartError("cleanup_failed"))?;
        let anchor = Anchor::start(inner.clone(), lock).await?;
        Ok(Anchored {
            inner,
            anchor,
            address,
        })
    }

    #[cfg(test)]
    async fn start_probed(
        config: ContainerConfig,
        host_has: impl Fn(Ipv4Addr) -> bool + Send + Sync + 'static,
        relay_up: impl Fn(SocketAddr) -> bool + Send + Sync + 'static,
        ready_within: Duration,
    ) -> Result<Anchored, StartError> {
        let hooks = Hooks {
            host_has: Arc::new(host_has),
            relay_up: Arc::new(relay_up),
            ready_within,
            reap_pause: Duration::from_millis(1),
            reap_within: REAP_WITHIN,
            address_poll: Duration::from_millis(20),
            ready_check: Duration::from_secs(5),
        };
        Self::start_with(config, hooks).await
    }

    pub fn address(&self) -> Ipv4Addr {
        self.inner
            .address
            .get()
            .copied()
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    pub fn health(&self) -> Option<watch::Receiver<Health>> {
        self.inner.gate.get().cloned()
    }

    pub async fn shutdown(&self) {
        let anchor = self
            .anchor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(anchor) = anchor {
            anchor.shutdown().await;
        }
    }

    pub fn owner_of(attempt: &Attempt) -> String {
        format!(
            "{}.{}.g{}.c{}",
            attempt.profile.0,
            attempt.run.0,
            attempt.generation.0,
            attempt.claim.map_or(0, |c| c.0)
        )
    }
}

impl Sandbox<ModelSpec> for ContainerSandbox {
    type Process = Box<dyn ModelProcess>;

    fn spawn(&self, spec: ModelSpec) -> Reply<'_, Self::Process> {
        Box::pin(async move {
            if !matches!(
                timeout(self.inner.hooks.ready_check, self.inner.ready()).await,
                Ok(true)
            ) {
                return Err(Error::Unavailable);
            }
            let process = self.inner.run_vm(Role::Model, Some(&spec)).await?;
            Ok(Box::new(process) as Box<dyn ModelProcess>)
        })
    }

    fn reap(&self, owner: &str) -> Reply<'_, ()> {
        let owner = owner.to_owned();
        Box::pin(async move {
            self.inner
                .reap_matching(Filter::Owner(&owner))
                .await
                .map(|_| ())
        })
    }
}
