//! Sandbox port isolating tool server processes, environment, network egress, and reaping.
//!
//! Adapters must enforce:
//! - Environment consists solely of `spec.env` (plus host-vetted `host_env`), with no inherited host env,
//!   home directory, host sockets, or DB mounts.
//! - Network egress goes solely through `spec.egress` (or none when `None`); no direct internet, DNS, or LAN bypass.
//! - `Handle::terminate` and `reap(owner)` succeed only after confirming the entire process group exited;
//!   unconfirmable state returns `Error::Unavailable` so the broker never re-executes.
//! - `spawn` fails closed if isolation cannot be established.

use bot_core::port::{Error, Reply};
use std::{net::SocketAddr, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite};

pub type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

pub struct Pipe {
    pub read: BoxRead,
    pub write: BoxWrite,
}

/// Per-call broker `CONNECT` proxy endpoint; `url` embeds per-call credentials for adapters (like `srt`) that wire it as an upstream proxy rather than passing env vars to the tool.
#[derive(Clone, Eq, PartialEq)]
pub struct ProxyEndpoint {
    pub addr: SocketAddr,
    pub url: String,
}

/// Omits per-call proxy credentials (`url`) from `Debug`.
impl std::fmt::Debug for ProxyEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyEndpoint")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub wall: Duration,
}

#[derive(Clone)]
pub struct SandboxSpec {
    /// Ledger-recorded ownership tag used for reaping and audit.
    pub owner: String,
    pub server: String,
    pub image: String,
    pub env: Vec<(String, String)>,
    /// Host-controlled non-secret variables (e.g. `BOT_EFFECT_URL`); adapters accept only an explicit allowlist.
    pub host_env: Vec<(String, String)>,
    pub egress: Option<ProxyEndpoint>,
    /// Allowed `host:port` list (declared ∩ operator cap) for defense-in-depth adapter enforcement.
    pub egress_hosts: Vec<String>,
    pub effects: bool,
    pub limits: Limits,
}

/// Logs environment variable names only so secret values never leak through `Debug`.
impl std::fmt::Debug for SandboxSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("SandboxSpec")
            .field("owner", &self.owner)
            .field("server", &self.server)
            .field("image", &self.image)
            .field("env", &names)
            .field(
                "host_env",
                &self
                    .host_env
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("egress", &self.egress)
            .field("egress_hosts", &self.egress_hosts)
            .field("effects", &self.effects)
            .field("limits", &self.limits)
            .finish()
    }
}

pub struct Process {
    /// MCP stdio pipe; stderr is capped and redacted by the adapter for diagnostics only.
    pub stdio: Pipe,
    pub effects: Option<Pipe>,
    pub handle: Box<dyn Handle>,
}

pub trait Handle: Send {
    fn terminate(self: Box<Self>) -> Reply<'static, ()>;
}

pub trait Sandbox: Send + Sync {
    fn spawn(&self, spec: SandboxSpec) -> Reply<'_, Process>;
    fn reap(&self, owner: &str) -> Reply<'_, ()>;
    /// Returns true for sandboxes that cannot pass extra FDs and must route effect callbacks through the broker proxy.
    fn effects_via_proxy(&self) -> bool {
        false
    }
    /// Pre-flight check run before resolving secrets (e.g., rejects `state: required` servers on stateless sandboxes).
    fn admit(&self, _server: &str, _image: &str) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_carries_environment_values_or_proxy_credentials() {
        let spec = SandboxSpec {
            owner: "r1.1.c1".into(),
            server: "demo".into(),
            image: "img1".into(),
            env: vec![("TOKEN".into(), "sekret-value-1".into())],
            host_env: vec![],
            egress: Some(ProxyEndpoint {
                addr: "127.0.0.1:9".parse().unwrap(),
                url: "http://bot:pw-123@127.0.0.1:9".into(),
            }),
            egress_hosts: vec![],
            effects: false,
            limits: Limits {
                wall: Duration::from_secs(1),
            },
        };
        let shown = format!("{spec:?}");
        assert!(
            shown.contains("TOKEN")
                && !shown.contains("sekret-value-1")
                && !shown.contains("pw-123"),
            "{shown}"
        );
    }
}
