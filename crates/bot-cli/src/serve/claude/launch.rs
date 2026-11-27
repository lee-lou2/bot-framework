//! Model sandbox launch strategy: Apple VM adapter in production or pre-opened test sandbox.
// VM boot order is type-enforced (`ContainerSandbox::start` -> `Anchored` -> bind relay on gateway IP -> `relay`),
// because the host-only gateway address is only bindable while the anchor VM is up.
use crate::{
    execution::{Apple, Provider},
    host::HostFile,
    install,
    serve::{runner::default_work, upstream::HttpsUpstream},
};
use bot_broker::model::Upstream;
use bot_core::sandbox::ModelSandbox;
use bot_runtime::model::{
    CliKind,
    container::{Anchored, ContainerConfig, ContainerSandbox, Entry, Health},
};
use std::{
    collections::BTreeSet,
    fs::DirBuilder,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone)]
pub enum Launch {
    #[cfg(any(test, feature = "testing"))]
    Ready {
        sandbox: Arc<ModelSandbox>,
        bind: SocketAddr,
        confined_reads: bool,
    },
    Container {
        config: Box<ContainerConfig>,
        relay_bind: Option<SocketAddr>,
    },
}

/// Builds production Apple VM parts from host config; refuses fallback if `sandbox.apple` or any required CLI image is missing.
pub fn operational(
    host: &HostFile,
    installs: Option<&Path>,
    uid: u32,
) -> Result<super::ClaudeParts, String> {
    let required: BTreeSet<CliKind> = host
        .catalog
        .models
        .values()
        .flatten()
        .filter_map(|alias| host.providers.get(alias).and_then(Provider::cli_kind))
        .collect();
    let apple = host.sandbox.apple.as_ref().ok_or_else(|| {
        let names = required
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let label = if names.is_empty() {
            "claude-cli".to_owned()
        } else {
            names
        };
        format!(
            "{label} alias is configured but host config is missing sandbox.apple (digest-pinned image and host-only network) — see docs/model-image.md"
        )
    })?;
    for kind in &required {
        if apple.image_for(*kind).is_none() {
            return Err(format!(
                "{} alias is configured but sandbox.apple is missing an image for `{}` (`sandbox.apple.images.{}`) — see docs/model-image.md",
                kind.as_str(),
                kind.program(),
                kind.program()
            ));
        }
    }
    let workspace = match &apple.work_root {
        Some(path) => path.clone(),
        None => {
            let base = install::default_base(installs, std::env::var("HOME").ok().as_deref())?;
            default_work(&base).join("models")
        }
    };
    private_dir(&workspace, uid)?;
    Ok(super::ClaudeParts {
        launch: Launch::Container {
            config: Box::new(container_config(apple, &workspace)?),
            relay_bind: apple.relay_bind,
        },
        upstream: Arc::new(HttpsUpstream::new()?) as Arc<dyn Upstream>,
        workspace,
        park: None,
    })
}

fn container_config(apple: &Apple, workspace: &Path) -> Result<ContainerConfig, String> {
    let anchor = apple.anchor_image().ok_or_else(|| {
        "sandbox.apple is missing a digest-pinned VM image (`image` or `images`)".to_owned()
    })?;
    let mut config =
        ContainerConfig::new(anchor, &apple.network, workspace).with_entry(Entry::model_image());
    for kind in [CliKind::Claude, CliKind::Codex, CliKind::Agy] {
        if let Some(image) = apple.image_for(kind) {
            config = config.with_image_for(kind.program(), image);
        }
    }
    if let Some(binary) = apple.binary.as_ref().and_then(|path| path.to_str()) {
        config.program = binary.to_owned();
    }
    Ok(match &apple.instance {
        Some(name) => config.named(name),
        None => config,
    })
}

fn private_dir(path: &Path, uid: u32) -> Result<(), String> {
    let shown = path.display();
    if std::fs::symlink_metadata(path).is_err() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| format!("failed to create model work directory ({shown}): {e}"))?;
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("failed to inspect model work directory ({shown}): {e}"))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{shown}: model work directory must be owned by current user with 0700 permissions"
        ));
    }
    Ok(())
}

/// Resolves the relay bind address; explicit `relay_bind` must match the anchor gateway IP exactly.
pub(super) fn relay_target(
    gateway: Ipv4Addr,
    relay_bind: Option<SocketAddr>,
) -> Result<SocketAddr, String> {
    match relay_bind {
        None => Ok(SocketAddr::new(IpAddr::V4(gateway), 0)),
        Some(bind) if bind.ip() == IpAddr::V4(gateway) => Ok(bind),
        Some(bind) => Err(format!(
            "sandbox.apple.relay_bind ({bind}) does not match anchor gateway IP ({gateway}) — omit it or use that address"
        )),
    }
}

pub(super) struct Vm {
    sandbox: Arc<ContainerSandbox>,
    stopping: Arc<AtomicBool>,
}

impl Vm {
    pub(super) async fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.sandbox.shutdown().await;
    }
}

pub(super) enum Pending {
    #[cfg(any(test, feature = "testing"))]
    Ready(Arc<ModelSandbox>),
    Anchored(Anchored),
}

impl Launch {
    pub(super) async fn prepare(self) -> Result<(SocketAddr, bool, Pending), String> {
        match self {
            #[cfg(any(test, feature = "testing"))]
            Self::Ready {
                sandbox,
                bind,
                confined_reads,
            } => Ok((bind, confined_reads, Pending::Ready(sandbox))),
            Self::Container { config, relay_bind } => {
                let anchored = ContainerSandbox::start(*config).await.map_err(|e| {
                    format!(
                        "failed to start model VM ({}) — check container service, image, network, and instance name (docs/model-image.md)",
                        e.0
                    )
                })?;
                match relay_target(anchored.address(), relay_bind) {
                    Ok(bind) => Ok((bind, true, Pending::Anchored(anchored))),
                    Err(error) => {
                        anchored.abort().await;
                        Err(error)
                    }
                }
            }
        }
    }
}

impl Pending {
    pub(super) async fn abort(self) {
        match self {
            #[cfg(any(test, feature = "testing"))]
            Self::Ready(_) => {}
            Self::Anchored(anchored) => anchored.abort().await,
        }
    }

    pub(super) async fn open(self, port: u16) -> Result<(Arc<ModelSandbox>, Option<Vm>), String> {
        match self {
            #[cfg(any(test, feature = "testing"))]
            Self::Ready(sandbox) => Ok((sandbox, None)),
            Self::Anchored(anchored) => {
                let vm = anchored.relay(port).await.map_err(|e| {
                    format!("model VM failed to confirm relay port {port} ({})", e.0)
                })?;
                let vm = Arc::new(vm);
                let stopping = Arc::new(AtomicBool::new(false));
                if let Some(mut health) = vm.health() {
                    let quiet = stopping.clone();
                    tokio::spawn(async move {
                        while health.changed().await.is_ok() {
                            if quiet.load(Ordering::SeqCst) {
                                continue;
                            }
                            match *health.borrow() {
                                Health::Up { generation } => {
                                    eprintln!("model VM anchor: up (generation {generation})");
                                }
                                Health::Down => eprintln!(
                                    "model VM anchor: down — new model calls will be rejected until recovery"
                                ),
                            }
                        }
                    });
                }
                Ok((
                    vm.clone() as Arc<ModelSandbox>,
                    Some(Vm {
                        sandbox: vm,
                        stopping,
                    }),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests;
