//! Host `AuthResolver` engine with surface policy checks, AEAD PG store, and single-flight PG refresh.

use super::{
    crypto::{MasterKey, aad_bytes},
    http::AuthHttp,
    payload::{CredentialPayload, looks_like_subscription_secret},
};
use bot_broker::resolve::HostPort;
use bot_core::{
    active::ActiveCheck,
    auth::{
        AuthError, AuthReply, AuthResolver, AuthSource, CallerSurface, CliVendor, CredentialKind,
        CredentialState, CredentialStore, ResolvedAuth, StoredCredential,
    },
    id::{ProfileId, SecretName},
    port::{SecretStore, SecretValue},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Semaphore, broadcast},
    task::JoinHandle,
};

const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
pub(super) const REFRESH_LEASE_MS: i64 = 35_000;
pub(super) const LOCK_WAIT: Duration = Duration::from_secs(45);
pub(super) const INFLIGHT_WAIT: Duration = Duration::from_secs(85);
const CACHE_REVALIDATE_TTL: Duration = Duration::from_secs(30);
const REFRESH_CONCURRENCY: usize = 4;
pub(super) const TRANSIENT_COOLDOWN: Duration = Duration::from_secs(15);
const SHUTDOWN_JOIN_WAIT: Duration = Duration::from_secs(10);

pub(super) type CredKey = (String, String);

pub(super) fn cred_key(profile: &ProfileId, name: &SecretName) -> CredKey {
    (profile.0.clone(), name.as_str().to_owned())
}

#[derive(Clone)]
pub(super) struct CachedEntry {
    pub(super) version: i64,
    pub(super) kind: CredentialKind,
    pub(super) has_refresh: bool,
    pub(super) subscription_provider: Option<String>,
    pub(super) issuer: Option<String>,
    pub(super) expires_ms: Option<i64>,
    pub(super) cached_at: Instant,
    pub(super) secret: Vec<u8>,
    pub(super) headers: Vec<(String, String)>,
}

pub(super) type InflightSender = broadcast::Sender<Result<CachedEntry, AuthError>>;

/// Host-side resolver unifying static secrets, OAuth 2.0, Google service accounts, and opt-in CLI subscriptions.
#[derive(Clone)]
pub struct HostAuthResolver {
    pub(super) inner: Arc<Inner>,
}

pub(super) struct Inner {
    pub(super) secrets: Arc<dyn SecretStore>,
    pub(super) creds: Option<Arc<dyn CredentialStore>>,
    pub(super) active: Arc<dyn ActiveCheck>,
    pub(super) http: AuthHttp,
    pub(super) egress: BTreeMap<ProfileId, Vec<HostPort>>,
    pub(super) allow_subscription_auth: BTreeMap<ProfileId, BTreeSet<String>>,
    pub(super) pinned_issuers: BTreeMap<String, String>,
    pub(super) provider_hosts: BTreeMap<String, BTreeSet<String>>,
    pub(super) register_secret: Arc<dyn Fn(&str) + Send + Sync>,
    pub(super) cache_ttl: Duration,
    pub(super) refresh_permits: Arc<Semaphore>,
    pub(super) cache: Mutex<BTreeMap<CredKey, CachedEntry>>,
    pub(super) cooldowns: Mutex<BTreeMap<CredKey, Instant>>,
    pub(super) inflight: Mutex<BTreeMap<CredKey, InflightSender>>,
    pub(super) refresh_tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl HostAuthResolver {
    /// Serve-path constructor requiring the redactor-wrapped `Watched` secret store at compile time.
    #[allow(clippy::too_many_arguments)]
    pub fn for_serve(
        secrets: Arc<crate::serve::watched::Watched>,
        creds: Option<Arc<dyn CredentialStore>>,
        active: Arc<dyn ActiveCheck>,
        http: AuthHttp,
        egress: BTreeMap<ProfileId, Vec<HostPort>>,
        allow_subscription_auth: BTreeMap<ProfileId, BTreeSet<String>>,
        pinned_issuers: BTreeMap<String, String>,
        provider_hosts: BTreeMap<String, BTreeSet<String>>,
    ) -> Self {
        let watched = secrets.clone();
        Self::build(
            secrets,
            creds,
            active,
            http,
            egress,
            allow_subscription_auth,
            pinned_issuers,
            provider_hosts,
            Arc::new(move |secret| watched.register(secret)),
            CACHE_REVALIDATE_TTL,
        )
    }

    pub fn new(
        secrets: Arc<dyn SecretStore>,
        creds: Option<Arc<dyn CredentialStore>>,
        active: Arc<dyn ActiveCheck>,
        http: AuthHttp,
        egress: BTreeMap<ProfileId, Vec<HostPort>>,
        allow_subscription_auth: BTreeMap<ProfileId, BTreeSet<String>>,
        register_secret: Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Self {
        Self::build(
            secrets,
            creds,
            active,
            http,
            egress,
            allow_subscription_auth,
            BTreeMap::new(),
            BTreeMap::new(),
            register_secret,
            CACHE_REVALIDATE_TTL,
        )
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn with_cache_ttl(self, cache_ttl: Duration) -> Self {
        Self::build(
            self.inner.secrets.clone(),
            self.inner.creds.clone(),
            self.inner.active.clone(),
            self.inner.http.clone(),
            self.inner.egress.clone(),
            self.inner.allow_subscription_auth.clone(),
            self.inner.pinned_issuers.clone(),
            self.inner.provider_hosts.clone(),
            self.inner.register_secret.clone(),
            cache_ttl,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        secrets: Arc<dyn SecretStore>,
        creds: Option<Arc<dyn CredentialStore>>,
        active: Arc<dyn ActiveCheck>,
        http: AuthHttp,
        egress: BTreeMap<ProfileId, Vec<HostPort>>,
        allow_subscription_auth: BTreeMap<ProfileId, BTreeSet<String>>,
        pinned_issuers: BTreeMap<String, String>,
        provider_hosts: BTreeMap<String, BTreeSet<String>>,
        register_secret: Arc<dyn Fn(&str) + Send + Sync>,
        cache_ttl: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                secrets,
                creds,
                active,
                http,
                egress,
                allow_subscription_auth,
                pinned_issuers,
                provider_hosts,
                register_secret,
                cache_ttl,
                refresh_permits: Arc::new(Semaphore::new(REFRESH_CONCURRENCY)),
                cache: Mutex::new(BTreeMap::new()),
                cooldowns: Mutex::new(BTreeMap::new()),
                inflight: Mutex::new(BTreeMap::new()),
                refresh_tasks: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Waits briefly on shutdown so detached in-flight refresh tasks finish committing rotated tokens.
    pub async fn shutdown(&self) {
        let handles: Vec<JoinHandle<()>> = {
            let mut guard = self
                .inner
                .refresh_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *guard)
        };
        if handles.is_empty() {
            return;
        }
        let _ = tokio::time::timeout(SHUTDOWN_JOIN_WAIT, async {
            for handle in handles {
                let _ = handle.await;
            }
        })
        .await;
    }

    fn wall_now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    pub(super) fn active(&self) -> &dyn ActiveCheck {
        self.inner.active.as_ref()
    }

    pub(super) fn http(&self) -> &AuthHttp {
        &self.inner.http
    }

    pub(super) fn profile_egress(&self, profile: &ProfileId) -> Option<&[HostPort]> {
        self.inner.egress.get(profile).map(Vec::as_slice)
    }

    pub(super) fn register_secret(&self) -> &(dyn Fn(&str) + Send + Sync) {
        self.inner.register_secret.as_ref()
    }

    pub(super) fn is_fresh(
        kind: CredentialKind,
        has_refresh: bool,
        expires_ms: Option<i64>,
        now_ms: i64,
    ) -> bool {
        match expires_ms {
            None => !(kind == CredentialKind::OAuth && has_refresh),
            Some(exp) => exp > now_ms.saturating_add(REFRESH_SKEW_MS),
        }
    }

    pub(super) fn check_pinned_metadata(
        &self,
        name: &SecretName,
        kind: CredentialKind,
        issuer: Option<&str>,
        subscription_provider: Option<&str>,
    ) -> Result<(), AuthError> {
        if kind == CredentialKind::OAuth
            && let Some(pinned) = self.inner.pinned_issuers.get(name.as_str())
            && issuer.map(|s| s.trim_end_matches('/')) != Some(pinned.as_str())
        {
            return Err(AuthError::Denied);
        }
        if let Some(provider) = subscription_provider
            && let Some(hosts) = self.inner.provider_hosts.get(name.as_str())
        {
            let expected = match provider {
                "claude" => "api.anthropic.com",
                "codex" => "chatgpt.com",
                _ => return Err(AuthError::Denied),
            };
            if !hosts.iter().all(|h| h.eq_ignore_ascii_case(expected)) {
                return Err(AuthError::Denied);
            }
        }
        Ok(())
    }

    fn subscription_allowed(&self, profile: &ProfileId, vendor: CliVendor) -> bool {
        self.inner
            .allow_subscription_auth
            .get(profile)
            .is_some_and(|set| set.contains(vendor.as_str()))
    }

    pub(super) fn check_surface_policy(
        &self,
        profile: &ProfileId,
        surface: CallerSurface,
        kind: CredentialKind,
        subscription_provider: Option<&str>,
    ) -> Result<(), AuthError> {
        match (kind, subscription_provider) {
            (CredentialKind::SubscriptionToken, Some(provider))
            | (CredentialKind::OAuth, Some(provider)) => match surface {
                CallerSurface::CliRelay(vendor)
                    if vendor.as_str() == provider
                        && self.subscription_allowed(profile, vendor) =>
                {
                    Ok(())
                }
                _ => Err(AuthError::Denied),
            },
            (CredentialKind::SubscriptionToken, None) => Err(AuthError::Denied),
            (CredentialKind::OAuth, None) => Ok(()),
            (CredentialKind::ServiceAccount, _) => match surface {
                CallerSurface::DirectApi
                | CallerSurface::CliRelay(_)
                | CallerSurface::RemoteMcp => Ok(()),
            },
        }
    }

    pub(super) fn payload_subscription_provider(payload: &CredentialPayload) -> Option<&str> {
        match payload {
            CredentialPayload::SubscriptionToken { provider, .. } => Some(provider.as_str()),
            CredentialPayload::OAuth {
                subscription_provider,
                ..
            } => subscription_provider.as_deref(),
            CredentialPayload::ServiceAccount { .. } => None,
        }
    }

    pub(super) fn decode_row(
        &self,
        key: &MasterKey,
        row: &StoredCredential,
    ) -> Result<CredentialPayload, AuthError> {
        let aad = aad_bytes(&row.profile, &row.name, row.kind, &row.token_endpoint_host);
        let plain = key.open(&aad, &row.nonce, &row.ciphertext)?;
        let payload: CredentialPayload =
            serde_json::from_slice(&plain).map_err(|_| AuthError::Invalid)?;
        if payload.kind() != row.kind || payload.validate().is_err() {
            return Err(AuthError::Invalid);
        }
        for secret in payload.secret_values() {
            (self.inner.register_secret)(secret);
        }
        Ok(payload)
    }

    async fn resolve_static(
        &self,
        profile: ProfileId,
        name: SecretName,
        surface: CallerSurface,
    ) -> Result<ResolvedAuth, AuthError> {
        let handle = self
            .inner
            .secrets
            .for_profile(profile)
            .await
            .map_err(AuthError::from)?;
        let secret = handle.resolve(name).await.map_err(AuthError::from)?;
        if surface == CallerSurface::DirectApi && looks_like_subscription_secret(&secret.0) {
            return Err(AuthError::Denied);
        }
        if let Ok(text) = std::str::from_utf8(&secret.0) {
            (self.inner.register_secret)(text);
        }
        Ok(ResolvedAuth::plain(secret))
    }

    /// Resolves a managed credential, coordinating in-process single-flight and PG advisory lock refresh.
    pub async fn resolve_managed(
        &self,
        profile: ProfileId,
        name: SecretName,
        expected_kind: CredentialKind,
        surface: CallerSurface,
        force_refresh: bool,
    ) -> Result<ResolvedAuth, AuthError> {
        let Some(creds) = self.inner.creds.as_ref() else {
            return Err(AuthError::Unavailable);
        };
        let key_pair = cred_key(&profile, &name);
        let now_ms = Self::wall_now_ms();
        let mut seen_version = 0i64;
        if !force_refresh {
            let cached = self
                .inner
                .cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key_pair)
                .cloned();
            if let Some(mut entry) = cached {
                seen_version = entry.version;
                if entry.kind != expected_kind {
                    return Err(AuthError::Denied);
                }
                self.check_surface_policy(
                    &profile,
                    surface,
                    entry.kind,
                    entry.subscription_provider.as_deref(),
                )?;
                self.check_pinned_metadata(
                    &name,
                    entry.kind,
                    entry.issuer.as_deref(),
                    entry.subscription_provider.as_deref(),
                )?;
                let mut still_valid = true;
                if entry.cached_at.elapsed() >= self.inner.cache_ttl {
                    match creds
                        .get_credential(profile.clone(), name.clone())
                        .await
                        .map_err(AuthError::from)?
                    {
                        None => {
                            self.evict(&key_pair);
                            return Err(AuthError::Missing);
                        }
                        Some(row) if row.state == CredentialState::NeedsLogin => {
                            self.evict(&key_pair);
                            return Err(AuthError::NeedsLogin);
                        }
                        Some(row)
                            if row.state == CredentialState::Active
                                && row.version == entry.version
                                && row.kind == expected_kind =>
                        {
                            entry.cached_at = Instant::now();
                            self.inner
                                .cache
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(key_pair.clone(), entry.clone());
                        }
                        Some(_) => still_valid = false,
                    }
                }
                if still_valid
                    && Self::is_fresh(entry.kind, entry.has_refresh, entry.expires_ms, now_ms)
                {
                    return Ok(ResolvedAuth {
                        secret: SecretValue(entry.secret),
                        headers: entry.headers,
                    });
                }
            }
        }
        let master = MasterKey::load(self.inner.secrets.as_ref()).await?;
        let Some(row) = creds
            .get_credential(profile.clone(), name.clone())
            .await
            .map_err(AuthError::from)?
        else {
            self.evict(&key_pair);
            return Err(AuthError::Missing);
        };
        if row.kind != expected_kind {
            return Err(AuthError::Denied);
        }
        seen_version = seen_version.max(row.version);
        let payload = self.decode_row(&master, &row)?;
        let issuer = match &payload {
            CredentialPayload::OAuth { issuer, .. } => issuer.as_deref(),
            _ => None,
        };
        self.check_surface_policy(
            &profile,
            surface,
            row.kind,
            Self::payload_subscription_provider(&payload),
        )?;
        self.check_pinned_metadata(
            &name,
            row.kind,
            issuer,
            Self::payload_subscription_provider(&payload),
        )?;
        if row.state == CredentialState::NeedsLogin {
            self.evict(&key_pair);
            tracing::warn!(
                profile = %profile.0,
                credential = %name.as_str(),
                code = "credential_needs_login",
                "managed credential requires operator login"
            );
            return Err(AuthError::NeedsLogin);
        }
        if !force_refresh
            && row.state == CredentialState::Active
            && let Ok(entry) = Self::to_cached(row.version, &payload)
            && Self::is_fresh(entry.kind, entry.has_refresh, entry.expires_ms, now_ms)
        {
            self.inner
                .cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key_pair, entry.clone());
            return Ok(ResolvedAuth {
                secret: SecretValue(entry.secret),
                headers: entry.headers,
            });
        }
        // SubscriptionToken does not refresh; return directly once validated.
        if matches!(payload, CredentialPayload::SubscriptionToken { .. }) {
            let entry = Self::to_cached(row.version, &payload)?;
            self.inner
                .cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key_pair, entry.clone());
            return Ok(ResolvedAuth {
                secret: SecretValue(entry.secret),
                headers: entry.headers,
            });
        }
        // Check in-memory transient cooldown before starting another refresh attempt.
        {
            let cooldowns = self
                .inner
                .cooldowns
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(until) = cooldowns.get(&key_pair)
                && *until > Instant::now()
            {
                return Err(AuthError::Unavailable);
            }
        }
        let entry = self
            .run_single_flight(
                profile,
                name,
                expected_kind,
                surface,
                seen_version,
                force_refresh,
            )
            .await?;
        Ok(ResolvedAuth {
            secret: SecretValue(entry.secret),
            headers: entry.headers,
        })
    }

    fn evict(&self, key: &CredKey) {
        self.inner
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

impl AuthResolver for HostAuthResolver {
    fn resolve(
        &self,
        profile: ProfileId,
        source: AuthSource,
        surface: CallerSurface,
        force_refresh: bool,
    ) -> AuthReply<'_, ResolvedAuth> {
        Box::pin(async move {
            match source {
                AuthSource::Secret(name) => self.resolve_static(profile, name, surface).await,
                AuthSource::Credential {
                    name,
                    expected_kind,
                } => {
                    self.resolve_managed(profile, name, expected_kind, surface, force_refresh)
                        .await
                }
            }
        })
    }
}
