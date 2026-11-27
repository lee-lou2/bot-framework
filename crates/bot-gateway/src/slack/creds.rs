//! Per-profile Slack credential resolution, `auth.test` team verification, and token-rejection cache eviction.
//!
//! Credentials are read strictly from `bot/<profile>/<name>` with no cross-profile fallback or merging.
use super::api::{AuthInfo, Body, Raw, SlackApi};
use crate::host::HostConfig;
use bot_core::{
    id::{ProfileId, SecretName},
    port::SecretStore,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const BOT_TTL: Duration = Duration::from_secs(30);

struct Entry {
    at: Instant,
    token: Arc<String>,
    auth: AuthInfo,
}

type Cache = Mutex<HashMap<(String, String), Entry>>;

pub enum BotCheck {
    Verified(Arc<String>, AuthInfo),
    Unreadable,
    Unverified,
    WrongTeam,
}

pub struct Credentials {
    secrets: Arc<dyn SecretStore>,
    host: Arc<HostConfig>,
    api: SlackApi,
    ttl: Duration,
    bot: Cache,
}

impl Credentials {
    pub fn new(secrets: Arc<dyn SecretStore>, host: Arc<HostConfig>, api: SlackApi) -> Self {
        Self {
            secrets,
            host,
            api,
            ttl: BOT_TTL,
            bot: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    pub fn host(&self) -> &Arc<HostConfig> {
        &self.host
    }

    async fn fetch(&self, profile: &ProfileId, name: &str) -> Option<Arc<String>> {
        let name = SecretName::parse(name)?;
        let handle = self.secrets.for_profile(profile.clone()).await.ok()?;
        let value = handle.resolve(name).await.ok()?;
        let text = String::from_utf8(value.0).ok()?;
        let text = text.trim().to_owned();
        (!text.is_empty()).then(|| Arc::new(text))
    }

    /// Resolves and caches a bot token, running `auth.test` whenever the value is unseen or rotated to verify `team_id`.
    pub async fn check_bot(&self, profile: &ProfileId, workspace: &str) -> BotCheck {
        let id = (profile.0.clone(), workspace.to_owned());
        if let Some(entry) = self.bot.lock().unwrap_or_else(|e| e.into_inner()).get(&id)
            && entry.at.elapsed() < self.ttl
        {
            return BotCheck::Verified(entry.token.clone(), entry.auth.clone());
        }
        let Some(registered) = self.host.slack.workspaces.get(workspace) else {
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
            return BotCheck::Verified(entry.token.clone(), entry.auth.clone());
        }
        let Some(auth) = self.api.auth_test(&fresh).await else {
            return BotCheck::Unverified;
        };
        if auth.team_id != registered.team_id {
            return BotCheck::WrongTeam;
        }
        self.bot.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id,
            Entry {
                at: Instant::now(),
                token: fresh.clone(),
                auth: auth.clone(),
            },
        );
        BotCheck::Verified(fresh, auth)
    }

    pub fn reject(&self, token: &str) {
        self.bot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, entry| entry.token.as_str() != token);
    }

    /// Calls Slack Web API and evicts the cached token on `TokenRejected` so the next attempt reloads from `SecretStore`.
    pub async fn call(&self, method: &str, token: &str, body: Body<'_>) -> Raw {
        let raw = self.api.call(method, token, body).await;
        if matches!(raw, Raw::TokenRejected) {
            tracing::warn!(
                method,
                "Slack rejected bot token — evicting cached token so the next attempt reloads from secret store"
            );
            self.reject(token);
        }
        raw
    }

    /// Best-effort UX API call that does not evict the core delivery token cache on failure.
    pub async fn call_passive(&self, method: &str, token: &str, body: Body<'_>) -> Raw {
        self.api.call(method, token, body).await
    }

    pub async fn bot_token(&self, profile: &ProfileId, workspace: &str) -> Option<Arc<String>> {
        match self.check_bot(profile, workspace).await {
            BotCheck::Verified(token, _) => Some(token),
            _ => None,
        }
    }

    /// Reads the app token fresh on each connect so rotations take effect on the next reconnect.
    pub async fn app_token(&self, profile: &ProfileId, workspace: &str) -> Option<Arc<String>> {
        let name = self
            .host
            .slack
            .workspaces
            .get(workspace)?
            .app_token
            .as_ref()?;
        self.fetch(profile, name).await
    }

    pub async fn signing_secret(
        &self,
        profile: &ProfileId,
        workspace: &str,
    ) -> Option<Arc<String>> {
        let name = self
            .host
            .slack
            .workspaces
            .get(workspace)?
            .signing_secret
            .as_ref()?;
        self.fetch(profile, name).await
    }
}
