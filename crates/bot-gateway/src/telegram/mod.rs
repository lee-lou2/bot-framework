//! Telegram Bot API long-polling gateway managing per-bot pollers, DM intake, approval callbacks, and reply delivery.
//!
//! Each poller serves one authenticated `(profile, bot)` installation with no cross-profile fallback; startup
//! rejects two installations sharing the same bot token, and HTTP 409 Conflict terminates polling immediately
//! without retrying or calling `deleteWebhook`.
pub mod api;
pub mod creds;
mod handle;
pub mod render;
pub mod send;

use crate::ingress::Ingress;
use api::{Raw, TelegramApi};
use bot_core::{
    id::ProfileId,
    ledger::CallLedger,
    port::{Broker, RunStore, SecretStore},
    profile::Adapter,
};
use creds::{BotCheck, Credentials};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::Arc,
    time::Duration,
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const POLL_TIMEOUT_SECS: u64 = 30;
const ERROR_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Debug, Eq, PartialEq)]
pub struct TelegramError(pub String);

impl fmt::Display for TelegramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TelegramError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Installation {
    pub profile: ProfileId,
    pub alias: String,
    pub bot_id: u64,
}

pub struct TelegramGateway {
    ingress: Arc<Ingress>,
    broker: Arc<dyn Broker>,
    ledger: Arc<dyn CallLedger>,
    creds: Arc<Credentials>,
    runs: Option<Arc<dyn RunStore>>,
    poll_timeout_secs: u64,
    error_backoff: Duration,
}

impl TelegramGateway {
    pub fn new(
        ingress: Arc<Ingress>,
        broker: Arc<dyn Broker>,
        ledger: Arc<dyn CallLedger>,
        secrets: Arc<dyn SecretStore>,
        api: TelegramApi,
    ) -> Self {
        let creds = Arc::new(Credentials::new(secrets, ingress.host().clone(), api));
        Self {
            ingress,
            broker,
            ledger,
            creds,
            runs: None,
            poll_timeout_secs: POLL_TIMEOUT_SECS,
            error_backoff: ERROR_BACKOFF,
        }
    }

    #[must_use]
    pub fn with_runs(mut self, runs: Arc<dyn RunStore>) -> Self {
        self.runs = Some(runs);
        self
    }

    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn with_poll_timing(mut self, poll_timeout_secs: u64, error_backoff: Duration) -> Self {
        self.poll_timeout_secs = poll_timeout_secs;
        self.error_backoff = error_backoff;
        self
    }

    pub fn sender(&self) -> send::TelegramSender {
        send::TelegramSender::new(self.creds.clone())
    }

    /// Validates host config, verifies each Telegram bot token via `getMe`, and rejects duplicate tokens.
    pub async fn plan(&self) -> Result<Vec<Installation>, TelegramError> {
        let host = self.ingress.host().clone();
        host.validate()
            .map_err(|error| TelegramError(error.to_string()))?;
        let routes = self
            .ingress
            .routes()
            .map_err(|error| TelegramError(error.to_string()))?;
        let mut pairs: BTreeMap<(ProfileId, String), u64> = BTreeMap::new();
        for snapshot in routes.profiles() {
            for binding in snapshot.bindings.values() {
                let Adapter::TelegramDm { bot } = &binding.source else {
                    continue;
                };
                let entry = host.telegram.bots.get(bot).ok_or_else(|| {
                    TelegramError(format!(
                        "{}: unregistered telegram bot alias {bot}",
                        snapshot.id.0
                    ))
                })?;
                if !entry.dm {
                    return Err(TelegramError(format!(
                        "{}: telegram bot {bot} does not allow DM ingress (dm: true)",
                        snapshot.id.0
                    )));
                }
                pairs.insert((snapshot.id.clone(), bot.clone()), entry.bot_id);
            }
        }
        let mut seen_tokens: HashMap<String, (ProfileId, String)> = HashMap::new();
        let mut installations = Vec::with_capacity(pairs.len());
        for ((profile, alias), bot_id) in pairs {
            let token = match self.creds.check_bot(&profile, &alias).await {
                BotCheck::Verified(token, _) => token,
                BotCheck::Unreadable => {
                    return Err(TelegramError(format!(
                        "{}/{alias}: missing or unreadable Telegram bot_token secret",
                        profile.0
                    )));
                }
                BotCheck::Unverified => {
                    return Err(TelegramError(format!(
                        "{}/{alias}: Telegram getMe failed — check bot_token and network connectivity",
                        profile.0
                    )));
                }
                BotCheck::WrongBot => {
                    return Err(TelegramError(format!(
                        "{}/{alias}: Telegram bot_token belongs to a different bot_id than host config",
                        profile.0
                    )));
                }
            };
            if let Some((first_profile, first_alias)) =
                seen_tokens.insert((*token).clone(), (profile.clone(), alias.clone()))
            {
                return Err(TelegramError(format!(
                    "Telegram bot_token is shared by {}/{first_alias} and {}/{alias}; each installation must use its own bot token",
                    first_profile.0, profile.0
                )));
            }
            installations.push(Installation {
                profile,
                alias,
                bot_id,
            });
        }
        Ok(installations)
    }

    pub async fn serve(self: Arc<Self>, stop: CancellationToken) -> Result<(), TelegramError> {
        let installs = self.plan().await?;
        if installs.is_empty() {
            stop.cancelled().await;
            return Ok(());
        }
        let mut tasks = JoinSet::new();
        for install in installs {
            let gateway = self.clone();
            let task_stop = stop.clone();
            tasks.spawn(async move { gateway.poll_loop(install, task_stop).await });
        }
        let mut failure = None;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failure = Some(error);
                    stop.cancel();
                    break;
                }
                Err(_) => {
                    failure = Some(TelegramError("telegram poller task panicked".into()));
                    stop.cancel();
                    break;
                }
            }
        }
        while tasks.join_next().await.is_some() {}
        failure.map_or(Ok(()), Err)
    }

    /// Long-polls `getUpdates` sequentially, advancing `offset` only through settled updates and aborting on 409 Conflict.
    pub async fn poll_once(
        &self,
        install: &Installation,
        offset: &mut Option<i64>,
        stop: &CancellationToken,
    ) -> Result<Duration, TelegramError> {
        let token = match self.creds.check_bot(&install.profile, &install.alias).await {
            BotCheck::Verified(token, _) => token,
            BotCheck::WrongBot => {
                return Err(TelegramError(format!(
                    "{}/{}: Telegram bot_token rotated to a different bot_id",
                    install.profile.0, install.alias
                )));
            }
            BotCheck::Unreadable | BotCheck::Unverified => return Ok(self.error_backoff),
        };
        let raw = tokio::select! {
            _ = stop.cancelled() => return Ok(Duration::ZERO),
            raw = self.creds.api().get_updates(&token, *offset, self.poll_timeout_secs) => raw,
        };
        match raw {
            Raw::Ok(Value::Array(updates)) => {
                for update in updates {
                    let Some(update_id) = update["update_id"].as_i64() else {
                        continue;
                    };
                    if stop.is_cancelled() {
                        return Ok(Duration::ZERO);
                    }
                    let settled = self.on_update(install, &update, stop).await;
                    if !settled {
                        // Stop before advancing offset past an unsettled update so transient DB errors retry in order.
                        return Ok(self.error_backoff);
                    }
                    *offset = Some(update_id.saturating_add(1));
                }
                Ok(Duration::ZERO)
            }
            Raw::Ok(_) => Ok(self.error_backoff),
            Raw::Conflict => {
                tracing::error!(
                    profile = %install.profile.0,
                    bot = %install.alias,
                    "Telegram getUpdates returned 409 Conflict — stopping poller without calling deleteWebhook"
                );
                Err(TelegramError(format!(
                    "{}/{}: Telegram getUpdates 409 Conflict — another consumer or webhook is active for this bot token",
                    install.profile.0, install.alias
                )))
            }
            Raw::TokenRejected => {
                self.creds.reject(&token);
                Ok(self.error_backoff)
            }
            Raw::RateLimited(wait) => Ok(wait),
            Raw::BeforeWrite | Raw::Unknown | Raw::Error(_, _) => Ok(self.error_backoff),
        }
    }

    async fn poll_loop(
        &self,
        install: Installation,
        stop: CancellationToken,
    ) -> Result<(), TelegramError> {
        let mut offset = None;
        loop {
            if stop.is_cancelled() {
                return Ok(());
            }
            let pause = self.poll_once(&install, &mut offset, &stop).await?;
            if !pause.is_zero() {
                tokio::select! {
                    _ = stop.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(pause) => {}
                }
            }
        }
    }
}
