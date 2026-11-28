//! Per-profile Telegram bot credential resolution, `getMe` bot ID verification, and cache eviction.
//!
//! Credentials are read strictly from `bot/<profile>/<name>` with no cross-profile fallback or merging.
use super::api::{BotInfo, Raw, TelegramApi};
use crate::host::HostConfig;
use bot_core::{
    id::{ProfileId, SecretName},
    port::SecretStore,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const BOT_TTL: Duration = Duration::from_secs(30);

struct Entry {
    at: Instant,
    token: Arc<String>,
    info: BotInfo,
}

type Cache = Mutex<HashMap<(String, String), Entry>>;

pub enum BotCheck {
    Verified(Arc<String>, BotInfo),
    Unreadable,
    Unverified,
    WrongBot,
}

pub struct Credentials {
    secrets: Arc<dyn SecretStore>,
    host: Arc<HostConfig>,
    api: TelegramApi,
    ttl: Duration,
    bot: Cache,
}

impl Credentials {
    pub fn new(secrets: Arc<dyn SecretStore>, host: Arc<HostConfig>, api: TelegramApi) -> Self {
        Self {
            secrets,
            host,
            api,
            ttl: BOT_TTL,
            bot: Mutex::new(HashMap::new()),
        }
    }

    pub fn host(&self) -> &Arc<HostConfig> {
        &self.host
    }

    pub fn api(&self) -> &TelegramApi {
        &self.api
    }

    async fn fetch(&self, profile: &ProfileId, name: &str) -> Option<Arc<String>> {
        let name = SecretName::parse(name)?;
        let handle = self.secrets.for_profile(profile.clone()).await.ok()?;
        let value = handle.resolve(name).await.ok()?;
        let text = String::from_utf8(value.0).ok()?;
        let text = text.trim().to_owned();
        (!text.is_empty()).then(|| Arc::new(text))
    }

    /// Resolves and caches a bot token, running `getMe` whenever unseen or rotated to verify `bot_id`.
    pub async fn check_bot(&self, profile: &ProfileId, alias: &str) -> BotCheck {
        let id = (profile.0.clone(), alias.to_owned());
        if let Some(entry) = self.bot.lock().unwrap_or_else(|e| e.into_inner()).get(&id)
            && entry.at.elapsed() < self.ttl
        {
            return BotCheck::Verified(entry.token.clone(), entry.info.clone());
        }
        let Some(registered) = self.host.telegram.bots.get(alias) else {
            return BotCheck::Unreadable;
        };
        let Some(fresh) = self.fetch(profile, &registered.bot_token).await else {
            return BotCheck::Unreadable;
        };
        if let Some(entry) = self
            .bot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&id)
            && *entry.token == *fresh
        {
            entry.at = Instant::now();
            return BotCheck::Verified(entry.token.clone(), entry.info.clone());
        }
        let Some(info) = self.api.get_me(&fresh).await else {
            return BotCheck::Unverified;
        };
        if info.id != registered.bot_id {
            return BotCheck::WrongBot;
        }
        self.bot.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id,
            Entry {
                at: Instant::now(),
                token: fresh.clone(),
                info: info.clone(),
            },
        );
        BotCheck::Verified(fresh, info)
    }

    pub fn reject(&self, token: &str) {
        self.bot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, entry| entry.token.as_str() != token);
    }

    /// Calls Telegram Bot API and evicts the cached token on `TokenRejected`.
    pub async fn call_json(&self, method: &str, token: &str, body: &Value) -> Raw {
        let raw = self.api.call_json(method, token, body).await;
        if matches!(raw, Raw::TokenRejected) {
            tracing::warn!(
                method,
                "Telegram rejected bot token — evicting cached token so the next attempt reloads from secret store"
            );
            self.reject(token);
        }
        raw
    }

    pub async fn bot_token(&self, profile: &ProfileId, alias: &str) -> Option<Arc<String>> {
        match self.check_bot(profile, alias).await {
            BotCheck::Verified(token, _) => Some(token),
            _ => None,
        }
    }
}
