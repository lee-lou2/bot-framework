//! Worker role wiring: active-revision guards, in-process broker, tool sandbox, model runners, and graceful quiesce.

#[cfg(any(test, feature = "testing"))]
use super::claude::ClaudeParts;
use super::{
    claude::{self, Claude},
    decide::LiveInstalled,
    guard::{GuardedBroker, GuardedRuns, StoreHandback},
    models,
    quiesce::{Quiescer, StartedCalls},
    tools::{self, ToolDeps, Tools},
    trace::Observe,
    watched::Watched,
};
use crate::{host::HostFile, secrets::admin::SecretAdmin, settings::Settings};
use bot_core::{
    active::ActiveCheck,
    id::Revision,
    ledger::CallLedger,
    port::{Broker, DelegationStore, MemoryStore, RunStore},
    profile::ProfileCatalog,
    schedule::{ScheduleRoutes, ScheduledReplies},
};
use bot_profile::Registry;
use bot_runtime::{Delivery, Deps as RuntimeDeps, NoRelays, Relays, Worker, WorkerConfig};
use bot_store::Store;
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{sync::watch, task::JoinSet};
use tokio_util::sync::CancellationToken;

/// Time given to in-flight runs after shutdown signal to finish or park cleanly.
const GRACE: Duration = Duration::from_secs(8);
/// Time allotted after `GRACE` to confirm process termination, revoke sessions, and hand back runs.
const CLEANUP: Duration = Duration::from_secs(10);
/// Outer drain bound in `serve::run`; must exceed `GRACE + CLEANUP` plus VM teardown (`Claude::stop`).
pub(super) const SHUTDOWN_WAIT: Duration = Duration::from_secs(40);
const _: () = assert!(GRACE.as_secs() + CLEANUP.as_secs() < SHUTDOWN_WAIT.as_secs());

pub struct Deps {
    pub host: Arc<HostFile>,
    pub settings: Settings,
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    pub raw_store: Arc<Store>,
    pub live_revision: Arc<RwLock<Revision>>,
    pub live_installed: LiveInstalled,
    /// Must be the redactor-wrapped store (`Observe.secrets`), never a raw backend.
    pub secrets: Arc<Watched>,
    pub secrets_admin: Arc<SecretAdmin>,
    pub observe: Arc<Observe>,
    pub uid: u32,
    pub stop: CancellationToken,
    pub active: Arc<dyn ActiveCheck>,
    #[cfg(any(test, feature = "testing"))]
    pub claude: Option<ClaudeParts>,
    #[cfg(any(test, feature = "testing"))]
    pub upstream: Option<Arc<dyn bot_broker::model::Upstream>>,
    pub routes: Arc<dyn ScheduleRoutes>,
}

pub struct Prepared {
    worker: Worker,
    stop: CancellationToken,
    tools: Tools,
    claude: Option<Claude>,
}

pub async fn prepare(deps: &Deps) -> Result<Prepared, String> {
    let missing = claude::missing_keys(
        &deps.host,
        &deps.registry,
        deps.secrets.as_ref(),
        &deps.settings.secrets,
    )
    .await;
    if !missing.is_empty() {
        return Err(format!(
            "cannot start worker because model service key(s) are missing from the secret store:\n{}",
            missing.join("\n")
        ));
    }
    let tools = tools::build(ToolDeps {
        host: &deps.host,
        settings: &deps.settings,
        registry: deps.registry.clone(),
        store: deps.store.clone(),
        raw_store: deps.raw_store.clone(),
        live_revision: deps.live_revision.clone(),
        live_installed: deps.live_installed.clone(),
        secrets: deps.secrets.clone(),
        secrets_admin: deps.secrets_admin.clone(),
        tracer: deps.observe.tracer(),
        uid: deps.uid,
        routes: deps.routes.clone(),
        active: deps.active.clone(),
    })
    .await?;
    let broker: Arc<dyn Broker> = Arc::new(GuardedBroker::new(
        tools.broker.clone(),
        deps.active.clone(),
    ));
    let claude_parts = claude::needed(&deps.host).then(|| {
        #[cfg(any(test, feature = "testing"))]
        if let Some(parts) = deps.claude.clone() {
            return Ok(parts);
        }
        let parts = claude::operational(&deps.host, deps.settings.installs.as_deref(), deps.uid);
        #[cfg(any(test, feature = "testing"))]
        let parts = parts.map(|mut parts| {
            if let Some(upstream) = &deps.upstream {
                parts.upstream = upstream.clone();
            }
            parts
        });
        parts
    });
    let claude = match claude_parts {
        Some(parts) => Some(
            claude::start(
                parts?,
                claude::Input {
                    host: &deps.host,
                    registry: deps.registry.clone(),
                    store: deps.store.clone(),
                    secrets: deps.secrets.clone(),
                    broker: broker.clone(),
                    preflight: tools.preflight.clone(),
                    runs: deps.store.clone() as Arc<dyn RunStore>,
                    active: deps.active.clone(),
                },
            )
            .await?,
        ),
        None => None,
    };
    let api = models::ApiParts {
        secrets: deps.secrets.clone(),
        gate: deps.active.clone(),
    };
    // Tear down any started VM front if model assembly fails afterwards.
    let models = match models::build(
        &deps.host,
        claude.as_ref().map(|c| c.runner.clone()),
        Some(&api),
    ) {
        Ok(models) => models,
        Err(error) => {
            if let Some(claude) = claude {
                claude.stop().await;
            }
            return Err(error);
        }
    };
    let relays: Arc<dyn Relays> = match &claude {
        Some(claude) => claude.relays.clone(),
        None => Arc::new(NoRelays),
    };
    let worker = Worker::new(
        RuntimeDeps {
            runs: Arc::new(
                GuardedRuns::new(deps.store.clone() as Arc<dyn RunStore>, deps.active.clone())
                    .with_handover(deps.store.clone()),
            ),
            memory: deps.store.clone() as Arc<dyn MemoryStore>,
            delegation: deps.store.clone() as Arc<dyn DelegationStore>,
            broker,
            ledger: deps.store.clone() as Arc<dyn CallLedger>,
            catalog: deps.registry.clone() as Arc<dyn ProfileCatalog>,
            models: Arc::new(models),
            relays,
            tracer: deps.observe.tracer(),
            gateway: None,
        },
        WorkerConfig {
            grace: GRACE,
            cleanup: CLEANUP,
            // 3s heartbeat keeps stop-request detection prompt well within the 30s lease.
            heartbeat: std::time::Duration::from_secs(3),
            ..WorkerConfig::default()
        },
    )
    .with_delivery(Delivery {
        replies: deps.store.clone() as Arc<dyn ScheduledReplies>,
        release: tools.delivery.clone(),
    })
    .with_handback(Arc::new(StoreHandback(deps.store.clone())))
    .with_quiesce(Arc::new(Quiescer::new(
        deps.store.clone() as Arc<dyn StartedCalls>,
        tools.runner.clone(),
        claude
            .as_ref()
            .map(|claude| (claude.sessions.clone(), claude.sandbox.clone())),
    )));
    Ok(Prepared {
        worker,
        stop: deps.stop.clone(),
        tools,
        claude,
    })
}

impl Prepared {
    pub async fn discard(self) {
        if let Some(claude) = self.claude {
            claude.stop().await;
        }
    }

    pub fn open(self, tasks: &mut JoinSet<Result<(), String>>) {
        let Self {
            worker,
            stop,
            tools,
            claude,
        } = self;
        tasks.spawn(async move {
            let (tx, rx) = watch::channel(false);
            tokio::spawn(async move {
                stop.cancelled().await;
                let _ = tx.send(true);
            });
            worker.run(rx).await;
            if let Some(claude) = claude {
                claude.stop().await;
            }
            // Drop after VM shutdown to release shared installation locks cleanly.
            drop(tools);
            Ok(())
        });
    }
}
