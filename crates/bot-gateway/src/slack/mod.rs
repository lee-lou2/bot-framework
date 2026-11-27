//! Slack Socket Mode gateway managing per-installation connections, reply delivery, and approval clicks.
//!
//! Each socket serves one authenticated `(profile, workspace)` installation with no cross-profile fallback; startup
//! rejects two profiles sharing the same app token because Slack splits events across connections.
pub mod api;
pub(crate) mod blocks;
pub mod cards;
pub mod creds;
mod events;
pub mod frame;
mod handle;
mod human;
pub mod send;
pub mod socket;
pub mod ux;

use crate::ingress::Ingress;
use api::SlackApi;
use bot_core::{
    id::ProfileId,
    ledger::CallLedger,
    port::{Broker, RunAdmin, RunStore, SecretStore},
    slack::BotThreads,
};
use creds::{BotCheck, Credentials};
use socket::Installation;
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const RECONCILE_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Eq, PartialEq)]
pub struct SlackError(pub String);

impl fmt::Display for SlackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SlackError {}

pub struct SlackGateway {
    ingress: Arc<Ingress>,
    broker: Arc<dyn Broker>,
    ledger: Arc<dyn CallLedger>,
    #[cfg(any(test, feature = "testing"))]
    secrets: Arc<dyn SecretStore>,
    creds: Arc<Credentials>,
    api: SlackApi,
    /// Evidence store for `thread_reply`; when absent, no thread replies are admitted.
    threads: Option<Arc<dyn BotThreads>>,
    ux: Option<Arc<ux::SlackUx>>,
    shutdown: CancellationToken,
}

impl SlackGateway {
    pub fn new(
        ingress: Arc<Ingress>,
        broker: Arc<dyn Broker>,
        ledger: Arc<dyn CallLedger>,
        secrets: Arc<dyn SecretStore>,
        api: SlackApi,
    ) -> Self {
        let creds = Arc::new(Credentials::new(
            secrets.clone(),
            ingress.host().clone(),
            api.clone(),
        ));
        Self {
            ingress,
            broker,
            ledger,
            #[cfg(any(test, feature = "testing"))]
            secrets,
            creds,
            api,
            threads: None,
            ux: None,
            shutdown: CancellationToken::new(),
        }
    }

    #[must_use]
    pub fn with_threads(mut self, threads: Arc<dyn BotThreads>) -> Self {
        self.threads = Some(threads);
        self
    }

    /// Attaches Slack progress indicators (👀/✅, stream status cards) and thread stop controls (button / "stop").
    #[must_use]
    pub fn with_ux(mut self, runs: Arc<dyn RunStore>, admin: Arc<dyn RunAdmin>) -> Self {
        self.ux = Some(Arc::new(ux::SlackUx::new(self.creds.clone(), runs, admin)));
        self
    }

    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn with_ux_timing(
        mut self,
        runs: Arc<dyn RunStore>,
        admin: Arc<dyn RunAdmin>,
        poll_every: Duration,
        watch_limit: Duration,
    ) -> Self {
        self.ux = Some(Arc::new(
            ux::SlackUx::new(self.creds.clone(), runs, admin)
                .with_watch_timing(poll_every, watch_limit),
        ));
        self
    }

    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn with_token_ttl(mut self, ttl: Duration) -> Self {
        self.creds = Arc::new(
            Credentials::new(
                self.secrets.clone(),
                self.ingress.host().clone(),
                self.api.clone(),
            )
            .with_ttl(ttl),
        );
        self
    }

    pub fn sender(&self) -> send::SlackSender {
        let sender = send::SlackSender::new(self.api.clone(), self.creds.clone());
        let sender = match &self.threads {
            Some(threads) => sender.with_threads(threads.clone()),
            None => sender,
        };
        match &self.ux {
            Some(ux) => sender.with_ux(ux.clone()),
            None => sender,
        }
    }

    /// Validates host config, resolves credentials, verifies `team_id`, and rejects duplicate app tokens across profiles.
    pub async fn plan(&self) -> Result<Vec<Installation>, SlackError> {
        let host = self.ingress.host().clone();
        // Enforce `HostConfig::validate` (rejecting unsupported receive modes) before reading credentials or calling Slack.
        host.validate()
            .map_err(|error| SlackError(error.to_string()))?;
        let routes = self
            .ingress
            .routes()
            .map_err(|error| SlackError(error.to_string()))?;
        let mut wanted: BTreeMap<(ProfileId, String), ()> = BTreeMap::new();
        for snapshot in routes.profiles() {
            for binding in snapshot.bindings.values() {
                use bot_core::profile::Adapter;
                if let Adapter::SlackChannel { workspace, .. } | Adapter::SlackDm { workspace } =
                    &binding.source
                {
                    wanted.insert((snapshot.id.clone(), workspace.clone()), ());
                }
            }
        }
        let mut installs = Vec::new();
        let mut tokens: HashMap<(String, String), ProfileId> = HashMap::new();
        for (profile, alias) in wanted.into_keys() {
            let who = format!("profile {} · workspace {alias}", profile.0);
            let Some(ws) = host.slack.workspaces.get(&alias) else {
                return Err(SlackError(format!("{who}: not registered in host config")));
            };
            let Some(app) = self.creds.app_token(&profile, &alias).await else {
                return Err(SlackError(format!(
                    "{who}: failed to read bot/app token (check secret names)"
                )));
            };
            if let Some(other) =
                tokens.insert((alias.clone(), app.as_str().to_owned()), profile.clone())
            {
                return Err(SlackError(format!(
                    "{who}: shares the same app token with profile {} — use a separate Slack app per profile so events are not split",
                    other.0
                )));
            }
            let auth = match self.creds.check_bot(&profile, &alias).await {
                BotCheck::Verified(_, auth) => auth,
                BotCheck::Unreadable => {
                    return Err(SlackError(format!(
                        "{who}: failed to read bot/app token (check secret names)"
                    )));
                }
                BotCheck::Unverified => {
                    return Err(SlackError(format!(
                        "{who}: failed to authenticate bot token with Slack"
                    )));
                }
                BotCheck::WrongTeam => {
                    return Err(SlackError(format!(
                        "{who}: token does not belong to the configured workspace"
                    )));
                }
            };
            installs.push(Installation {
                profile,
                alias,
                team: ws.team_id.clone(),
                app_id: ws.app_id.clone(),
                bot_user: auth.user_id,
            });
        }
        Ok(installs)
    }

    /// Runs sockets until `stop`, reconciling installations on profile hot-reloads while preserving running ones on error.
    pub async fn serve(self: Arc<Self>, stop: CancellationToken) -> Result<(), SlackError> {
        let desired = self.plan().await?;
        let mut seen = self.ingress.routes().ok();
        let mut running: HashMap<(ProfileId, String), Running> = HashMap::new();
        self.reconcile(&mut running, desired, &stop);
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = tokio::time::sleep(RECONCILE_EVERY) => {}
            }
            let current = self.ingress.routes().ok();
            let unchanged = match (&seen, &current) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if unchanged {
                continue;
            }
            seen = current;
            match self.plan().await {
                Ok(next) => self.reconcile(&mut running, next, &stop),
                Err(error) => {
                    tracing::error!(%error, "failed to plan Slack installations for updated profile set — keeping previous installations");
                }
            }
        }
        self.shutdown.cancel();
        for running in running.into_values() {
            running.stop.cancel();
            let _ = running.task.await;
        }
        Ok(())
    }

    fn reconcile(
        self: &Arc<Self>,
        running: &mut HashMap<(ProfileId, String), Running>,
        desired: Vec<Installation>,
        parent: &CancellationToken,
    ) {
        let wanted: HashMap<(ProfileId, String), Installation> = desired
            .into_iter()
            .map(|install| ((install.profile.clone(), install.alias.clone()), install))
            .collect();
        running.retain(|key, run| {
            let keep = wanted.get(key) == Some(&run.install);
            if !keep {
                run.stop.cancel();
            }
            keep
        });
        for (key, install) in wanted {
            running
                .entry(key)
                .or_insert_with(|| self.spawn_socket(install, parent));
        }
    }

    fn spawn_socket(
        self: &Arc<Self>,
        install: Installation,
        parent: &CancellationToken,
    ) -> Running {
        let stop = parent.child_token();
        let handler: socket::Handler = {
            let (gateway, install, stop) = (self.clone(), install.clone(), stop.clone());
            Arc::new(move |frame| {
                let (gateway, install, stop) = (gateway.clone(), install.clone(), stop.clone());
                Box::pin(async move { gateway.on_frame_with_stop(&install, frame, &stop).await })
            })
        };
        let task = tokio::spawn({
            let (api, creds, install, stop) = (
                self.api.clone(),
                self.creds.clone(),
                install.clone(),
                stop.clone(),
            );
            async move {
                if let Err(socket::Fatal(reason)) =
                    socket::run(api, creds, install, handler, stop).await
                {
                    tracing::error!(%reason, "stopped Slack socket");
                }
            }
        });
        Running {
            install,
            stop,
            task,
        }
    }
}

struct Running {
    install: Installation,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
