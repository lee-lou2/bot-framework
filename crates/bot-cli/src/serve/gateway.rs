//! Gateway role wiring: prepares CLI, HTTP/OpenAI, and Slack listeners before activation, then opens ingress.
// Two-phase startup (`prepare` then `open`) ensures listener/credential/Slack validation failures leave
// the published active revision untouched.
use super::{runner, watched::Watched};
use crate::{
    host::HostFile,
    settings::Settings,
    validate::{
        entrance_secrets, has_private_triggers, missing_named, weak_http_keys, weak_openai_keys,
    },
};
use bot_broker::{
    private::{PrivateGate, PrivateParts},
    rate::RateConfig,
    seal::JcsSealer,
    sys::SystemClock,
};
use bot_core::{
    active::ActiveCheck,
    api::ApiRuns,
    cards::CardOutbox,
    intake::{Outbox, PrivateCall, PrivateDecision, PrivateIntake},
    ledger::{CallLedger, PrivateLedger},
    port::{Broker, EffectJournal, Error, Reply, RunAdmin, RunStore},
    profile::{Adapter, ProfileBundles, ProfileCatalog},
    slack::BotThreads,
    trace::Tracer,
};
use bot_gateway::{
    cli::{CliGateway, socket},
    delivery::Deliverer,
    http::HttpGateway,
    ingress::Ingress,
    slack::{SlackGateway, api::SlackApi, cards::CardDeliverer},
    telegram::{TelegramGateway, api::TelegramApi, cards::TelegramCardDeliverer},
};
use bot_profile::{Registry, SchemaValidator};
use bot_store::Store;
use std::sync::Arc;
use tokio::{net::TcpListener, net::UnixListener, task::JoinSet};
use tokio_util::sync::CancellationToken;

pub struct Deps {
    pub settings: Settings,
    pub host: Arc<HostFile>,
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    /// Redaction-watched secret store (`Observe.secrets`); raw stores must never be passed to roles.
    pub secrets: Arc<Watched>,
    pub uid: u32,
    pub stop: CancellationToken,
    pub decisions: Arc<dyn Broker>,
    pub tracer: Arc<dyn Tracer>,
    #[cfg(any(test, feature = "testing"))]
    pub slack_base: Option<String>,
    #[cfg(any(test, feature = "testing"))]
    pub telegram_base: Option<String>,
    pub active: Arc<dyn ActiveCheck>,
}

struct UnwiredPrivate;

impl PrivateIntake for UnwiredPrivate {
    fn receive(&self, _: PrivateCall) -> Reply<'_, PrivateDecision> {
        Box::pin(std::future::ready(Err(Error::Unavailable)))
    }
}

/// Wires `PrivateGate` when any profile declares private DM triggers, holding tool install locks for ingress lifetime.
async fn private_intake(deps: &Deps) -> Result<Arc<dyn PrivateIntake>, String> {
    if !deps
        .registry
        .active()
        .values()
        .any(|snapshot| has_private_triggers(snapshot))
    {
        return Ok(Arc::new(UnwiredPrivate));
    }
    let egress = crate::auth::cli::host_egress_map(&deps.host)?;
    let watched = deps.secrets.clone();
    let auth: Arc<dyn bot_core::auth::AuthResolver> = Arc::new(crate::auth::HostAuthResolver::new(
        deps.secrets.clone(),
        Some(deps.store.clone() as Arc<dyn bot_core::auth::CredentialStore>),
        deps.active.clone(),
        crate::auth::http::AuthHttp::new()?,
        egress,
        std::collections::BTreeMap::new(),
        Arc::new(move |secret| watched.register(secret)),
    ));
    let live_runner = runner::build(
        &deps.host,
        &deps.settings,
        &deps.secrets,
        auth,
        deps.store.clone() as Arc<dyn EffectJournal>,
        deps.uid,
        deps.active.clone(),
    )
    .await?;
    let gate = PrivateGate::new(
        PrivateParts {
            catalog: deps.registry.clone() as Arc<dyn ProfileCatalog>,
            ledger: deps.store.clone() as Arc<dyn PrivateLedger>,
            secrets: deps.secrets.clone(),
            sealer: Arc::new(JcsSealer::new(Arc::new(SchemaValidator::new()))),
            runner: live_runner.tool_runner(),
            tracer: deps.tracer.clone(),
            clock: Arc::new(SystemClock),
        },
        RateConfig::default(),
    );
    Ok(Arc::new(Held {
        gate,
        _runner: live_runner,
    }))
}

/// Keeps tool install shared locks alive as long as `Ingress` handles in-flight private DM calls.
struct Held {
    gate: PrivateGate,
    _runner: runner::LiveRunner,
}

impl PrivateIntake for Held {
    fn receive(&self, call: PrivateCall) -> Reply<'_, PrivateDecision> {
        self.gate.receive(call)
    }
}

pub struct Prepared {
    ingress: Arc<Ingress>,
    store: Arc<Store>,
    secrets: Arc<Watched>,
    stop: CancellationToken,
    slack: Option<Arc<SlackGateway>>,
    telegram: Option<Arc<TelegramGateway>>,
    cli: Option<UnixListener>,
    http: Option<TcpListener>,
}

pub async fn prepare(deps: &Deps) -> Result<Prepared, String> {
    let private = private_intake(deps).await?;
    let ingress = Arc::new(
        Ingress::new(
            deps.registry.clone() as Arc<dyn ProfileCatalog>,
            Arc::new(deps.host.gateway.clone()),
            deps.store.clone() as Arc<dyn RunStore>,
            private,
        )
        .with_tracer(deps.tracer.clone())
        .with_active(deps.active.clone()),
    );
    let routes = ingress.routes().map_err(|e| e.to_string())?;
    let (mut cli, mut http, mut slack, mut telegram) = (false, false, false, false);
    let mut problems = Vec::new();
    let mut snapshots = Vec::new();
    for snapshot in routes.profiles() {
        for binding in snapshot.bindings.values() {
            match binding.source {
                Adapter::Cli { .. } => cli = true,
                Adapter::Http { .. } | Adapter::OpenAi { .. } => http = true,
                Adapter::SlackChannel { .. } | Adapter::SlackDm { .. } => slack = true,
                Adapter::TelegramDm { .. } | Adapter::TelegramGroups { .. } => telegram = true,
            }
        }
        snapshots.push(snapshot.clone());
    }
    drop(routes);
    problems.extend(weak_openai_keys(&*deps.secrets, &snapshots, &deps.host.gateway).await);
    for snapshot in snapshots {
        let names = entrance_secrets(&snapshot, &deps.host.gateway);
        problems.extend(
            missing_named(&*deps.secrets, &snapshot.id, names, &deps.settings.secrets).await,
        );
        problems.extend(weak_http_keys(&*deps.secrets, &snapshot, &deps.host.gateway).await);
    }
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    let slack_gateway = if slack {
        #[cfg(any(test, feature = "testing"))]
        let slack_base = deps.slack_base.as_deref();
        #[cfg(not(any(test, feature = "testing")))]
        let slack_base: Option<&str> = None;
        let gateway = Arc::new(
            SlackGateway::new(
                ingress.clone(),
                deps.decisions.clone(),
                deps.store.clone() as Arc<dyn CallLedger>,
                deps.secrets.clone(),
                SlackApi::new(slack_base),
            )
            .with_threads(deps.store.clone() as Arc<dyn BotThreads>)
            .with_ux(
                deps.store.clone() as Arc<dyn RunStore>,
                deps.store.clone() as Arc<dyn RunAdmin>,
            ),
        );
        gateway.plan().await.map_err(|e| e.to_string())?;
        Some(gateway)
    } else {
        None
    };
    let telegram_gateway = if telegram {
        #[cfg(any(test, feature = "testing"))]
        let telegram_base = deps.telegram_base.as_deref();
        #[cfg(not(any(test, feature = "testing")))]
        let telegram_base: Option<&str> = None;
        let gateway = Arc::new(
            TelegramGateway::new(
                ingress.clone(),
                deps.decisions.clone(),
                deps.store.clone() as Arc<dyn CallLedger>,
                deps.secrets.clone(),
                TelegramApi::new(telegram_base),
            )
            .with_runs(deps.store.clone() as Arc<dyn RunStore>),
        );
        gateway.plan().await.map_err(|e| e.to_string())?;
        Some(gateway)
    } else {
        None
    };
    let cli_listener = if cli {
        let path = socket::socket_path(
            deps.settings
                .socket
                .as_deref()
                .or(deps.host.gateway.cli.socket.as_deref()),
            &|name| std::env::var(name).ok(),
            deps.uid,
        )?;
        let listener = socket::bind(&path, deps.uid)
            .map_err(|e| format!("failed to bind CLI socket ({}): {e}", path.display()))?;
        eprintln!("gateway: CLI socket {}", path.display());
        Some(listener)
    } else {
        None
    };
    let http_listener = if http {
        let addr = deps.host.gateway.listen();
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("failed to bind HTTP gateway ({addr}): {e}"))?;
        eprintln!("gateway: HTTP {addr}");
        Some(listener)
    } else {
        None
    };
    if !(cli || http || slack || telegram) {
        eprintln!("gateway: no ingress bound (profiles define no bindings)");
    }
    Ok(Prepared {
        ingress,
        store: deps.store.clone(),
        secrets: deps.secrets.clone(),
        stop: deps.stop.clone(),
        slack: slack_gateway,
        telegram: telegram_gateway,
        cli: cli_listener,
        http: http_listener,
    })
}

impl Prepared {
    /// Starts serving ingress; call only after active revision and seeds are committed.
    pub fn open(self, tasks: &mut JoinSet<Result<(), String>>) {
        let Self {
            ingress,
            store,
            secrets,
            stop,
            slack,
            telegram,
            cli,
            http,
        } = self;
        if let Some(listener) = cli {
            let gateway = Arc::new(CliGateway::new(
                ingress.clone(),
                store.clone() as Arc<dyn RunStore>,
            ));
            let stop = stop.clone();
            tasks.spawn(async move {
                gateway.serve(listener, stop).await;
                Ok(())
            });
        }
        if let Some(listener) = http {
            let gateway = Arc::new(
                HttpGateway::new(ingress.clone(), store.clone() as Arc<dyn RunStore>, secrets)
                    .with_openai(
                        store.clone() as Arc<dyn ApiRuns>,
                        store.clone() as Arc<dyn ProfileBundles>,
                    ),
            );
            let stop = stop.clone();
            tasks.spawn(async move {
                gateway
                    .serve(listener, stop)
                    .await
                    .map_err(|e| format!("HTTP gateway stopped: {e}"))
            });
        }
        if let Some(gateway) = slack {
            let deliverer = Deliverer::new(
                store.clone() as Arc<dyn Outbox>,
                ingress.clone(),
                Arc::new(gateway.sender()),
            );
            let cards = CardDeliverer::new(
                store.clone() as Arc<dyn CardOutbox>,
                ingress.clone(),
                gateway.sender(),
            );
            tasks.spawn({
                let stop = stop.clone();
                async move { gateway.serve(stop).await.map_err(|e| e.to_string()) }
            });
            tasks.spawn({
                let stop = stop.clone();
                async move {
                    cards.run(stop).await;
                    Ok(())
                }
            });
            tasks.spawn({
                let stop = stop.clone();
                async move {
                    deliverer.run(stop).await;
                    Ok(())
                }
            });
        }
        if let Some(gateway) = telegram {
            let deliverer = Deliverer::new(
                store.clone() as Arc<dyn Outbox>,
                ingress.clone(),
                Arc::new(gateway.sender()),
            );
            let cards =
                TelegramCardDeliverer::new(store as Arc<dyn CardOutbox>, ingress, gateway.sender());
            tasks.spawn({
                let stop = stop.clone();
                async move { gateway.serve(stop).await.map_err(|e| e.to_string()) }
            });
            tasks.spawn({
                let stop = stop.clone();
                async move {
                    cards.run(stop).await;
                    Ok(())
                }
            });
            tasks.spawn(async move {
                deliverer.run(stop).await;
                Ok(())
            });
        }
    }
}
