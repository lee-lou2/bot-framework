//! Tool-executing broker assembly (sandboxed MCP runner, schedules, and `LiveManager`).
use super::{
    decide::LiveInstalled,
    manage::{LiveManager, LiveManagerParts},
    runner::{self, LiveRunner},
    unusable::unusable_tools,
    watched::Watched,
};
use crate::{host::HostFile, secrets::admin::SecretAdmin, settings::Settings};
use bot_broker::{
    ports::ToolRunner,
    seal::JcsSealer,
    service::{BrokerService, Config, Parts, preflight::Preflight},
    sys::{OsEntropy, SystemClock},
};
use bot_core::{
    active::ActiveCheck,
    id::Revision,
    ledger::CallLedger,
    manage::ProfileManager,
    port::{AuditLog, Broker, EffectJournal, MemoryStore},
    profile::ProfileCatalog,
    schedule::{ScheduleDelivery, ScheduleRoutes, ScheduledReplies, UserSchedules},
    trace::Tracer,
};
use bot_profile::{Registry, SchemaValidator};
use bot_store::Store;
use std::sync::{Arc, RwLock};

pub struct ToolDeps<'a> {
    pub host: &'a HostFile,
    pub settings: &'a Settings,
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    pub raw_store: Arc<Store>,
    pub live_revision: Arc<RwLock<Revision>>,
    pub live_installed: LiveInstalled,
    /// Accepts `Watched` only so every read tool credential enters the redactor before use.
    pub secrets: Arc<Watched>,
    pub secrets_admin: Arc<SecretAdmin>,
    pub tracer: Arc<dyn Tracer>,
    pub uid: u32,
    pub routes: Arc<dyn ScheduleRoutes>,
    pub active: Arc<dyn ActiveCheck>,
}

pub struct Tools {
    pub broker: Arc<dyn Broker>,
    pub delivery: Arc<dyn ScheduleDelivery>,
    pub preflight: Arc<dyn Preflight>,
    pub runner: Arc<dyn ToolRunner>,
    _live: LiveRunner,
}

pub async fn build(deps: ToolDeps<'_>) -> Result<Tools, String> {
    let ToolDeps {
        host,
        settings,
        registry,
        store,
        raw_store,
        live_revision,
        live_installed,
        secrets,
        secrets_admin,
        tracer,
        uid,
        routes,
        active,
    } = deps;
    let ledger = store.clone() as Arc<dyn CallLedger>;
    let live_runner = runner::build(
        host,
        settings,
        &secrets,
        store.clone() as Arc<dyn EffectJournal>,
        uid,
        active.clone(),
    )
    .await?;
    let runner = live_runner.tool_runner();
    let embedder = crate::embeddings::build(host.embeddings.as_ref(), secrets.clone(), active)?;
    // Missing tool secrets warn by name without blocking startup; the broker refuses calls with `secret_missing`.
    if !host.servers.is_empty() {
        for item in unusable_tools(&*secrets, &registry.active()).await {
            eprintln!("{}", item.warning());
        }
    }
    let schedules = store.clone() as Arc<dyn UserSchedules>;
    let replies = store.clone() as Arc<dyn ScheduledReplies>;
    let tool_runner = runner.clone();
    let mut service = BrokerService::new(
        Parts {
            catalog: registry.clone() as Arc<dyn ProfileCatalog>,
            ledger,
            effects: store.clone() as Arc<dyn EffectJournal>,
            memory: store.clone() as Arc<dyn MemoryStore>,
            audit: store as Arc<dyn AuditLog>,
            runner,
            sealer: Arc::new(JcsSealer::new(Arc::new(SchemaValidator::new()))),
            tracer,
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    )
    .with_schedules(schedules)
    .with_replies(replies)
    .with_routes(routes);
    if let Some(embedder) = embedder {
        service = service.with_embedder(embedder);
    }

    if let Some(profiles_root) = settings.profiles.clone() {
        let host_copy = match &settings.host_config {
            Some(path) => {
                let mut h = crate::host::load(path).map_err(|e| e.to_string())?;
                h.pin_uids()?;
                h
            }
            None => {
                let mut demo = crate::host::demo(uid);
                demo.attach_demo_server(Some(&profiles_root));
                demo.pin_uids()?;
                demo
            }
        };
        let manager: Arc<dyn ProfileManager> = Arc::new(LiveManager::new(LiveManagerParts {
            settings: settings.clone(),
            profiles_root,
            host_path: settings.host_config.clone(),
            host: host_copy,
            registry,
            raw_store,
            live_revision,
            live_installed,
            live_runner: live_runner.clone(),
            secrets_admin,
            watched: secrets,
            uid,
        }));
        service = service.with_manager(manager);
    }

    let broker = Arc::new(service);
    Ok(Tools {
        broker: broker.clone(),
        delivery: broker.clone(),
        preflight: broker,
        runner: tool_runner,
        _live: live_runner,
    })
}
