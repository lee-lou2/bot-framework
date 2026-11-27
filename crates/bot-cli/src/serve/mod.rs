//! `bot serve` role assembly, preflight validation, activation publication, and coordinated shutdown.
//!
//! Assembles `gateway`, `scheduler`, `worker`, and `admin` (`all` runs all four). Broker runs
//! in-process inside `worker`, so `--role broker` is rejected before starting anything.

mod activation;
mod admin;
pub mod claude;
mod decide;
mod gateway;
mod grants;
mod guard;
mod manage;
mod models;
mod quiesce;
pub mod remote;
mod restore;
mod runner;
mod scheduler;
pub mod seed;
mod token_file;
mod tools;
mod trace;
mod unusable;
#[doc(hidden)]
pub mod upstream;
mod watched;
mod worker;

use self::activation::{commit_with_refresh, plan_activation};
pub(crate) use self::runner::refuse_required_state;
#[cfg(any(test, feature = "testing"))]
use crate::secrets::admin::SecretAdmin;
use crate::{
    cli::Role,
    host::HostFile,
    load::{check_entrances, load, secret_admin},
    settings::Settings,
};
use bot_core::{
    active::{ActiveCheck, ActiveSet},
    command::{MemorySeed, TriggerSpec},
    id::{ProfileId, Revision},
    port::{Error, SecretStore},
    profile::{ActiveRevisions, ProfileCatalog},
    schedule::ScheduleRoutes,
};
use bot_gateway::schedule_routes::HostRoutes;
use bot_profile::ProfileSet;
use bot_runtime::schedule_set;
use bot_store::{Config, Store};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::IsTerminal,
    sync::{Arc, RwLock},
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Validates model alias configuration using the same `models::build` rules as worker startup without reading keys or sending requests.
pub(crate) fn check_model_aliases(host: &HostFile) -> Result<(), String> {
    struct Open;
    impl ActiveCheck for Open {
        fn verify(&self) -> bot_core::port::Reply<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    let claude: Arc<dyn bot_runtime::model::ModelRunner> = Arc::new(bot_runtime::model::DemoModel);
    let api = models::ApiParts {
        secrets: trace::inert_secrets(),
        gate: Arc::new(Open),
    };
    models::build(host, Some(claude), Some(&api)).map(|_| ())
}

fn not_assembled(role: Role) -> Option<&'static str> {
    match role {
        Role::Broker => Some(
            "broker (tool execution and approvals) runs inside worker — use --role worker instead",
        ),
        Role::All | Role::Gateway | Role::Scheduler | Role::Worker | Role::Admin => None,
    }
}

fn explain(error: Error) -> &'static str {
    match error {
        Error::Conflict => "another revision is already published",
        Error::Unavailable => "store is unavailable",
        Error::Invalid => "invalid format",
        _ => "rejected by store",
    }
}

/// Active set published atomically: profile set revision bound to the host authority fingerprint,
/// per-profile revisions, memory seeds, static cron schedules, and pre-approval grants.
pub struct Publication {
    pub revision: Revision,
    pub profiles: BTreeMap<ProfileId, Revision>,
    pub seeds: Vec<MemorySeed>,
    pub schedules: BTreeMap<ProfileId, Vec<TriggerSpec>>,
    pub grants: Vec<bot_core::grant::GrantSpec>,
}

/// Activation revision domain tag. Bump only when activation payload semantics change (forces `--activate` on upgrade).
/// v1: profiles only. v2: adds static schedules. v3: adds pre-approval grants.
const ACTIVATION_VERSION: &[u8] = b"bot-active-v3\0";

pub fn activation_revision(set: &Revision, mapping: &[u8; 32]) -> Revision {
    let mut hash = Sha256::new();
    hash.update(ACTIVATION_VERSION);
    hash.update((set.0.len() as u64).to_be_bytes());
    hash.update(set.0.as_bytes());
    hash.update(mapping);
    Revision(format!("{:x}", hash.finalize()))
}

impl Publication {
    /// Drops grant issuance unless `--activate` (`explicit`) is set so plain starts never open cardless writes.
    #[must_use]
    pub fn for_run(mut self, explicit: bool) -> (Self, usize) {
        if explicit {
            return (self, 0);
        }
        let unissued = self.grants.len();
        self.grants.clear();
        (self, unissued)
    }

    pub fn print_grants(&self) {
        for grant in &self.grants {
            println!("Pre-approval scope {}", grant.scope_line());
        }
    }

    pub fn of(
        set: &ProfileSet,
        host: &HostFile,
        revisions: &BTreeMap<String, Revision>,
    ) -> Result<Self, String> {
        let authority = host.authority_fingerprint(revisions)?;
        let grants = grants::grants_of(set, revisions)?;
        Ok(Self {
            revision: activation_revision(set.revision(), &authority),
            grants,
            profiles: set
                .active()
                .iter()
                .map(|(id, snapshot)| (id.clone(), snapshot.revision.clone()))
                .collect(),
            seeds: set.active().values().flat_map(|s| seed::seeds(s)).collect(),
            schedules: set
                .active()
                .iter()
                .map(|(id, snapshot)| (id.clone(), schedule_set(snapshot).triggers))
                .filter(|(_, triggers)| !triggers.is_empty())
                .collect(),
        })
    }
}

fn watch_signals(stop: CancellationToken) {
    tokio::spawn(async move {
        let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match term {
                    Ok(mut term) => { term.recv().await; }
                    Err(_) => std::future::pending::<()>().await,
                }
            } => {}
        }
        stop.cancel();
    });
}

/// Test-only seams for substituting model runners, secrets, shutdown tokens, or endpoints. Never compiled into production builds.
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
#[derive(Default)]
pub struct Overrides {
    pub claude: Option<claude::ClaudeParts>,
    pub secrets: Option<Arc<dyn SecretStore>>,
    pub secret_admin: Option<Arc<SecretAdmin>>,
    pub stop: Option<CancellationToken>,
    pub slack_base: Option<String>,
    pub upstream: Option<Arc<dyn bot_broker::model::Upstream>>,
    pub store_config: Option<Config>,
}

pub async fn run(settings: &Settings, roles: &[Role], explicit: bool) -> Result<(), String> {
    #[cfg(any(test, feature = "testing"))]
    {
        run_inner(settings, roles, explicit, Overrides::default()).await
    }
    #[cfg(not(any(test, feature = "testing")))]
    {
        run_inner(settings, roles, explicit).await
    }
}

#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub async fn run_with(
    settings: &Settings,
    roles: &[Role],
    explicit: bool,
    overrides: Overrides,
) -> Result<(), String> {
    run_inner(settings, roles, explicit, overrides).await
}

async fn run_inner(
    settings: &Settings,
    roles: &[Role],
    explicit: bool,
    #[cfg(any(test, feature = "testing"))] overrides: Overrides,
) -> Result<(), String> {
    let roles = Role::expand(roles);
    let blocked: Vec<String> = roles
        .iter()
        .filter_map(|role| not_assembled(*role).map(|why| format!("  {}: {why}", role.name())))
        .collect();
    if !blocked.is_empty() {
        return Err(format!(
            "cannot start requested role(s); nothing was started:\n{}",
            blocked.join("\n")
        ));
    }
    let loaded = load(settings)?;
    check_entrances(loaded.set.active().clone(), &loaded.host)?;
    if loaded.host.demo {
        eprintln!(
            "BOT_HOST_CONFIG is not set; using built-in demo host (CLI accounts owner, team = current user)"
        );
    }
    let seed_problems: Vec<String> = loaded
        .set
        .active()
        .values()
        .flat_map(|snapshot| seed::problems(snapshot))
        .collect();
    if !seed_problems.is_empty() {
        return Err(seed_problems.join("\n"));
    }
    let revisions = crate::install::measure_all(&loaded.host.servers, &loaded.host.remote_servers)?;
    #[cfg(any(test, feature = "testing"))]
    let secrets_admin = match (overrides.secret_admin, overrides.secrets) {
        (Some(admin), _) => admin,
        (None, Some(secrets)) => Arc::new(SecretAdmin::custom(secrets)),
        (None, None) => secret_admin(settings)?,
    };
    #[cfg(not(any(test, feature = "testing")))]
    let secrets_admin = secret_admin(settings)?;
    let observe = Arc::new(
        trace::start(
            &loaded.host,
            &loaded.registry,
            secrets_admin.clone() as Arc<dyn SecretStore>,
        )
        .await?,
    );
    // All roles read secrets through the redactor-watched wrapper so rotated values are masked before use.
    let secrets = observe.secrets.clone();
    let url = settings
        .database_url
        .as_deref()
        .ok_or("BOT_DATABASE_URL is required (PostgreSQL connection URL)")?;
    #[cfg(any(test, feature = "testing"))]
    let mut store_config = overrides.store_config.clone().unwrap_or_default();
    #[cfg(not(any(test, feature = "testing")))]
    let mut store_config = Config::default();
    if let Some(max) = settings.db_max_connections {
        store_config.max_connections = max;
    }
    let store = Store::connect(url, store_config)
        .await
        .map_err(|e| format!("failed to connect to store: {}", explain(e.into_core())))?;
    store
        .migrate()
        .await
        .map_err(|e| format!("store migration failed: {}", explain(e.into_core())))?;
    let (publication, unissued) =
        Publication::of(&loaded.set, &loaded.host, &revisions)?.for_run(explicit);
    if unissued > 0 {
        tracing::info!(
            count = unissued,
            "pre-approval declarations require explicit --activate to issue or update grants (existing grants remain readable)"
        );
    }
    let plan = plan_activation(&store, &publication, explicit).await?;
    // Persist new bundles and restore pinned historical revisions before publishing; `restored` holds strong refs until exit.
    let mut restored = restore::prepare(
        &store,
        &loaded.registry,
        &loaded.set,
        &publication.profiles,
        &bot_profile::Limits::default(),
    )
    .await?;
    // Roles use a store handle fenced to `live_revision` so live skill/MCP activation updates every role in this process together.
    let live_revision = Arc::new(RwLock::new(publication.revision.clone()));
    let fenced = Arc::new(store.for_live_activation(live_revision.clone()));
    let store = Arc::new(store);
    let active: Arc<dyn ActiveCheck> = Arc::new(ActiveSet::live(
        fenced.clone() as Arc<dyn ActiveRevisions>,
        live_revision.clone(),
    ));
    // Fence telemetry exporters so stale processes stop exporting once observability config changes.
    observe.set_gate(
        {
            let active = active.clone();
            Arc::new(move || {
                let active = active.clone();
                Box::pin(async move { active.verify().await.is_ok() })
            })
        },
        std::time::Duration::from_secs(3),
    );
    let routes: Arc<dyn ScheduleRoutes> = Arc::new(HostRoutes::new(
        loaded.registry.clone() as Arc<dyn ProfileCatalog>,
        Arc::new(loaded.host.gateway.clone()),
    ));
    let live_installed = decide::LiveInstalled::from_revisions(&revisions);
    let decisions = decide::broker(
        loaded.registry.clone(),
        fenced.clone(),
        live_installed.clone(),
        observe.tracer(),
        routes.clone(),
    );
    #[cfg(any(test, feature = "testing"))]
    let (own_signals, stop) = (overrides.stop.is_none(), overrides.stop.unwrap_or_default());
    #[cfg(not(any(test, feature = "testing")))]
    let (own_signals, stop) = (true, CancellationToken::new());
    let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();
    let prepared_gateway = if roles.contains(&Role::Gateway) {
        let deps = gateway::Deps {
            settings: settings.clone(),
            host: loaded.host.clone(),
            registry: loaded.registry.clone(),
            store: fenced.clone(),
            secrets: secrets.clone(),
            uid: loaded.uid,
            stop: stop.clone(),
            decisions: decisions.clone(),
            tracer: observe.tracer(),
            #[cfg(any(test, feature = "testing"))]
            slack_base: overrides.slack_base.clone(),
            active: active.clone(),
        };
        Some(gateway::prepare(&deps).await?)
    } else {
        None
    };
    let prepared_admin = if roles.contains(&Role::Admin) {
        let deps = admin::Deps {
            host: loaded.host.clone(),
            registry: loaded.registry.clone(),
            store: fenced.clone(),
            observe: observe.clone(),
            uid: loaded.uid,
            stop: stop.clone(),
            decisions: decisions.clone(),
            token_file: settings.admin_token_file.clone(),
            terminal: std::io::stdout().is_terminal(),
            active: active.clone(),
        };
        Some(admin::prepare(&deps).await?)
    } else {
        None
    };
    let prepared_scheduler = roles.contains(&Role::Scheduler).then(|| {
        scheduler::prepare(&scheduler::Deps {
            registry: loaded.registry.clone(),
            store: fenced.clone(),
            observe: observe.clone(),
            stop: stop.clone(),
            routes: routes.clone(),
        })
    });
    let prepared_worker = if roles.contains(&Role::Worker) {
        let deps = worker::Deps {
            host: loaded.host.clone(),
            settings: settings.clone(),
            registry: loaded.registry.clone(),
            store: fenced.clone(),
            raw_store: store.clone(),
            live_revision: live_revision.clone(),
            live_installed: live_installed.clone(),
            secrets: secrets.clone(),
            secrets_admin: secrets_admin.clone(),
            observe: observe.clone(),
            uid: loaded.uid,
            stop: stop.clone(),
            active: active.clone(),
            #[cfg(any(test, feature = "testing"))]
            claude: overrides.claude.clone(),
            #[cfg(any(test, feature = "testing"))]
            upstream: overrides.upstream.clone(),
            routes: routes.clone(),
        };
        Some(worker::prepare(&deps).await?)
    } else {
        None
    };
    let mut prepared_admin = prepared_admin;
    // Publish token file before committing active revision so a failed commit rolls back the file and tears down any started VM.
    let committed = async {
        if let Some(admin) = prepared_admin.as_mut() {
            admin.publish()?;
        }
        commit_with_refresh(
            &store,
            &loaded.registry,
            &publication,
            &plan,
            &loaded.set,
            &mut restored,
        )
        .await
    }
    .await;
    let wrote = match committed {
        Ok(wrote) => wrote,
        Err(error) => {
            if let Some(prepared) = prepared_worker {
                prepared.discard().await;
            }
            return Err(error);
        }
    };
    if wrote {
        tracing::info!("published active revision");
    }
    if restored.restored() > 0 {
        tracing::info!(
            count = restored.restored(),
            "restored pinned historical profile revisions"
        );
    }
    if let Some(prepared) = prepared_gateway {
        prepared.open(&mut tasks);
    }
    if let Some(prepared) = prepared_admin {
        prepared.open(&mut tasks);
    }
    if let Some(prepared) = prepared_scheduler {
        prepared.open(&mut tasks);
    }
    if let Some(prepared) = prepared_worker {
        prepared.open(&mut tasks);
    }
    if own_signals {
        watch_signals(stop.clone());
    }
    // Periodically re-read declared secrets to refresh the redactor in split-role processes that do not resolve them directly.
    let rereader = settings.secrets_reread.map(|every| {
        tokio::spawn(watched::reread(
            secrets.clone(),
            watched::declared(&loaded.host, &loaded.registry),
            watched::Pace::new(every),
            stop.clone(),
        ))
    });
    let mut failure: Option<String> = None;
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            joined = tasks.join_next() => match joined {
                None => break,
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(error))) => { failure = Some(error); stop.cancel(); break; }
                Some(Err(_)) => { failure = Some("role task terminated abnormally".into()); stop.cancel(); break; }
            }
        }
    }
    let drain = async { while tasks.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(worker::SHUTDOWN_WAIT, drain).await;
    if let Some(rereader) = rereader {
        rereader.abort();
    }
    observe.flush().await;
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev(text: &str) -> Revision {
        Revision(text.into())
    }

    #[test]
    fn the_activation_revision_binds_the_profile_set_and_the_mapping_fingerprint() {
        let set = rev("set-1");
        let original = activation_revision(&set, &[1; 32]);
        assert_eq!(
            original,
            activation_revision(&set, &[1; 32]),
            "deterministic"
        );
        assert_ne!(
            original,
            activation_revision(&set, &[2; 32]),
            "different mapping must change revision"
        );
        assert_ne!(
            original,
            activation_revision(&rev("set-2"), &[1; 32]),
            "different profile set must change revision"
        );
        assert_eq!(original.0.len(), 64);
        assert_ne!(
            activation_revision(&rev("a"), &[0; 32]),
            activation_revision(&rev("a\0"), &[0; 32])
        );
    }
}
