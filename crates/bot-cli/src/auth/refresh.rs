//! Single-flight PG-leased credential refresh execution, crash quarantine, and CAS commit.

use super::{
    crypto::{MasterKey, aad_bytes},
    engine::{
        CachedEntry, CredKey, HostAuthResolver, INFLIGHT_WAIT, Inner, LOCK_WAIT, REFRESH_LEASE_MS,
        TRANSIENT_COOLDOWN, cred_key,
    },
    oauth::{self, RefreshError},
    payload::{CredentialPayload, canonical_host_port},
    service_account::{self, ExchangeError},
};
use bot_core::{
    auth::{
        AuthError, CallerSurface, CredentialKind, CredentialLease, CredentialState,
        CredentialUpsert,
    },
    id::{ProfileId, SecretName},
    port::Error,
};
use std::{sync::Arc, time::Instant};
use tokio::sync::broadcast;

struct InflightGuard {
    inner: Arc<Inner>,
    key: CredKey,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inner
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

impl HostAuthResolver {
    pub(super) fn to_cached(
        version: i64,
        payload: &CredentialPayload,
    ) -> Result<CachedEntry, AuthError> {
        let kind = payload.kind();
        let subscription_provider = Self::payload_subscription_provider(payload).map(str::to_owned);
        let cached_at = Instant::now();
        match payload {
            CredentialPayload::SubscriptionToken { token, .. } => Ok(CachedEntry {
                version,
                kind,
                has_refresh: false,
                subscription_provider,
                issuer: None,
                expires_ms: None,
                cached_at,
                secret: token.as_bytes().to_vec(),
                headers: Vec::new(),
            }),
            CredentialPayload::OAuth {
                access_token,
                refresh_token,
                expires_ms,
                headers,
                issuer,
                ..
            } => Ok(CachedEntry {
                version,
                kind,
                has_refresh: refresh_token.is_some(),
                subscription_provider,
                issuer: issuer.clone(),
                expires_ms: *expires_ms,
                cached_at,
                secret: access_token.as_bytes().to_vec(),
                headers: headers.clone(),
            }),
            CredentialPayload::ServiceAccount {
                access_token: Some(token),
                expires_ms,
                ..
            } => Ok(CachedEntry {
                version,
                kind,
                has_refresh: true,
                subscription_provider,
                issuer: None,
                expires_ms: *expires_ms,
                cached_at,
                secret: token.as_bytes().to_vec(),
                headers: Vec::new(),
            }),
            CredentialPayload::ServiceAccount {
                access_token: None, ..
            } => Err(AuthError::Unavailable),
        }
    }

    pub(super) async fn run_single_flight(
        &self,
        profile: ProfileId,
        name: SecretName,
        expected_kind: CredentialKind,
        surface: CallerSurface,
        min_version: i64,
        force_refresh: bool,
    ) -> Result<CachedEntry, AuthError> {
        let key_pair = cred_key(&profile, &name);
        let mut rx = {
            let mut map = self
                .inner
                .inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(tx) = map.get(&key_pair) {
                tx.subscribe()
            } else {
                let (tx, rx) = broadcast::channel(1);
                map.insert(key_pair.clone(), tx.clone());
                let this = self.clone();
                let kp = key_pair.clone();
                // Detached task guarantees caller cancellation never aborts a rotating token commit.
                let handle = tokio::spawn(async move {
                    let _guard = InflightGuard {
                        inner: this.inner.clone(),
                        key: kp,
                    };
                    let result = this
                        .refresh_under_pg_lock(
                            profile,
                            name,
                            expected_kind,
                            surface,
                            min_version,
                            force_refresh,
                        )
                        .await;
                    drop(_guard);
                    let _ = tx.send(result);
                });
                let mut tasks = self
                    .inner
                    .refresh_tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                tasks.retain(|h| !h.is_finished());
                tasks.push(handle);
                rx
            }
        };
        tokio::time::timeout(INFLIGHT_WAIT, rx.recv())
            .await
            .map_err(|_| AuthError::Unavailable)?
            .map_err(|_| AuthError::Unavailable)?
    }

    async fn refresh_under_pg_lock(
        &self,
        profile: ProfileId,
        name: SecretName,
        expected_kind: CredentialKind,
        surface: CallerSurface,
        seen_version: i64,
        force_refresh: bool,
    ) -> Result<CachedEntry, AuthError> {
        let Some(creds) = self.inner.creds.as_ref() else {
            return Err(AuthError::Unavailable);
        };
        let _permit = self
            .inner
            .refresh_permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AuthError::Unavailable)?;
        let key_pair = cred_key(&profile, &name);
        let master = MasterKey::load(self.inner.secrets.as_ref()).await?;
        let mut lease = creds
            .lock_credential(profile.clone(), name.clone(), LOCK_WAIT)
            .await
            .map_err(AuthError::from)?;
        let outcome = self
            .refresh_with_lease(
                &mut *lease,
                &master,
                &profile,
                &name,
                expected_kind,
                surface,
                seen_version,
                force_refresh,
            )
            .await;
        let _ = lease.release().await;
        match &outcome {
            Ok(entry) => {
                self.inner
                    .cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key_pair.clone(), entry.clone());
                self.inner
                    .cooldowns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key_pair);
            }
            Err(AuthError::Unavailable) => {
                self.inner
                    .cooldowns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key_pair, Instant::now() + TRANSIENT_COOLDOWN);
            }
            Err(_) => {
                self.inner
                    .cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key_pair);
            }
        }
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn refresh_with_lease(
        &self,
        lease: &mut dyn CredentialLease,
        master: &MasterKey,
        profile: &ProfileId,
        name: &SecretName,
        expected_kind: CredentialKind,
        surface: CallerSurface,
        seen_version: i64,
        force_refresh: bool,
    ) -> Result<CachedEntry, AuthError> {
        let Some(row) = lease.get().await.map_err(AuthError::from)? else {
            return Err(AuthError::Missing);
        };
        if row.kind != expected_kind {
            return Err(AuthError::Denied);
        }
        let now_ms = lease.now_ms().await.map_err(AuthError::from)?;
        let payload = self.decode_row(master, &row)?;
        let issuer = match &payload {
            CredentialPayload::OAuth { issuer, .. } => issuer.as_deref(),
            _ => None,
        };
        self.check_surface_policy(
            profile,
            surface,
            row.kind,
            Self::payload_subscription_provider(&payload),
        )?;
        self.check_pinned_metadata(
            name,
            row.kind,
            issuer,
            Self::payload_subscription_provider(&payload),
        )?;

        if row.state == CredentialState::NeedsLogin {
            return Err(AuthError::NeedsLogin);
        }
        // If another holder crashed while rotating an OAuth refresh token, quarantine to needs_login.
        if row.state == CredentialState::Refreshing && row.kind == CredentialKind::OAuth {
            let _ = lease
                .mark_needs_login(row.version, row.nonce.clone(), "interrupted_refresh")
                .await;
            tracing::warn!(
                profile = %profile.0,
                credential = %name.as_str(),
                code = "credential_needs_login",
                "previous OAuth refresh was interrupted; quarantined credential"
            );
            return Err(AuthError::NeedsLogin);
        }
        // Double-check after acquiring PG lock: adopt a token rotated by another worker.
        let adopted_newer = force_refresh && row.version > seen_version;
        if row.state == CredentialState::Active
            && (!force_refresh || adopted_newer)
            && let Ok(entry) = Self::to_cached(row.version, &payload)
            && Self::is_fresh(entry.kind, entry.has_refresh, entry.expires_ms, now_ms)
        {
            return Ok(entry);
        }
        // First activation fence before marking the row refreshing.
        self.active().verify().await.map_err(|e| match e {
            Error::Stale => AuthError::Stale,
            other => AuthError::from(other),
        })?;

        let refresh_nonce = bot_core::random::random_hex(16);
        let refreshing_version = lease
            .begin_refresh(
                row.version,
                row.nonce.clone(),
                refresh_nonce,
                REFRESH_LEASE_MS,
            )
            .await
            .map_err(AuthError::from)?;
        let egress_cap = self.profile_egress(profile);
        // Second activation fence immediately before any outbound token-exchange POST.
        if let Err(err) = self.active().verify().await {
            let _ = lease
                .abort_refresh(refreshing_version, row.nonce.clone(), "stale_activation")
                .await;
            return Err(match err {
                Error::Stale => AuthError::Stale,
                other => AuthError::from(other),
            });
        }

        let next_payload = match payload {
            CredentialPayload::OAuth {
                token_endpoint,
                client_id,
                client_secret,
                access_token: _,
                refresh_token: Some(rt),
                expires_ms: _,
                scope,
                headers,
                body_format,
                subscription_provider,
                resource,
                issuer,
            } => {
                match oauth::refresh_token(
                    self.http(),
                    &token_endpoint,
                    body_format,
                    &client_id,
                    client_secret.as_deref(),
                    &rt,
                    scope.as_deref(),
                    resource.as_deref(),
                    egress_cap,
                    self.register_secret(),
                )
                .await
                {
                    Ok(resp) => {
                        let expires_ms = resp
                            .expires_in_secs
                            .map(|s| now_ms.saturating_add(s * 1000));
                        CredentialPayload::OAuth {
                            token_endpoint,
                            client_id,
                            client_secret,
                            access_token: resp.access_token,
                            refresh_token: Some(resp.refresh_token.unwrap_or(rt)),
                            expires_ms,
                            scope: resp.scope.or(scope),
                            headers,
                            body_format,
                            subscription_provider,
                            resource,
                            issuer,
                        }
                    }
                    Err(RefreshError::InvalidGrant) => {
                        let _ = lease
                            .mark_needs_login(
                                refreshing_version,
                                row.nonce.clone(),
                                "invalid_grant",
                            )
                            .await;
                        return Err(AuthError::NeedsLogin);
                    }
                    Err(RefreshError::Transient(reason)) => {
                        tracing::warn!(
                            profile = %profile.0,
                            credential = %name.as_str(),
                            code = "credential_refresh_failed",
                            reason,
                            "OAuth refresh failed with transient error"
                        );
                        let _ = lease
                            .abort_refresh(refreshing_version, row.nonce.clone(), reason)
                            .await;
                        return Err(AuthError::Unavailable);
                    }
                }
            }
            CredentialPayload::OAuth {
                refresh_token: None,
                ..
            } => {
                let _ = lease
                    .mark_needs_login(refreshing_version, row.nonce.clone(), "no_refresh_token")
                    .await;
                return Err(AuthError::NeedsLogin);
            }
            CredentialPayload::ServiceAccount {
                client_email,
                private_key_pem,
                token_uri,
                scope,
                ..
            } => {
                match service_account::exchange_jwt(
                    self.http(),
                    &client_email,
                    &private_key_pem,
                    &token_uri,
                    &scope,
                    now_ms,
                    egress_cap,
                    self.register_secret(),
                )
                .await
                {
                    Ok((access_token, expires_ms)) => CredentialPayload::ServiceAccount {
                        client_email,
                        private_key_pem,
                        token_uri,
                        scope,
                        access_token: Some(access_token),
                        expires_ms: Some(expires_ms),
                    },
                    Err(ExchangeError::InvalidGrant) => {
                        let _ = lease
                            .mark_needs_login(
                                refreshing_version,
                                row.nonce.clone(),
                                "invalid_grant",
                            )
                            .await;
                        return Err(AuthError::NeedsLogin);
                    }
                    Err(ExchangeError::InvalidConfig) => {
                        tracing::warn!(
                            profile = %profile.0,
                            credential = %name.as_str(),
                            code = "credential_refresh_failed",
                            reason = "invalid_config",
                            "service-account JWT exchange rejected configuration"
                        );
                        let _ = lease
                            .abort_refresh(refreshing_version, row.nonce.clone(), "invalid_config")
                            .await;
                        return Err(AuthError::Invalid);
                    }
                    Err(ExchangeError::Transient) => {
                        tracing::warn!(
                            profile = %profile.0,
                            credential = %name.as_str(),
                            code = "credential_refresh_failed",
                            reason = "transient_error",
                            "service-account JWT exchange failed transiently"
                        );
                        let _ = lease
                            .abort_refresh(refreshing_version, row.nonce.clone(), "transient_error")
                            .await;
                        return Err(AuthError::Unavailable);
                    }
                }
            }
            CredentialPayload::SubscriptionToken { .. } => {
                let _ = lease
                    .abort_refresh(refreshing_version, row.nonce.clone(), "non_refreshable")
                    .await;
                return Err(AuthError::Invalid);
            }
        };

        let host_port = match next_payload.token_endpoint() {
            Some(url) => canonical_host_port(url, self.http().allow_loopback_http())
                .map_err(|_| AuthError::Invalid)?,
            None => String::new(),
        };
        let aad = aad_bytes(profile, name, next_payload.kind(), &host_port);
        let plain = serde_json::to_vec(&next_payload).map_err(|_| AuthError::Invalid)?;
        let (nonce, ciphertext) = master.seal(&aad, &plain)?;
        let upsert = CredentialUpsert {
            profile: profile.clone(),
            name: name.clone(),
            kind: next_payload.kind(),
            token_endpoint_host: host_port,
            nonce,
            ciphertext,
            expires_ms: next_payload.expires_ms(),
        };
        let next_version = match lease
            .commit_refresh(refreshing_version, row.nonce.clone(), upsert.clone())
            .await
        {
            Ok(v) => v,
            Err(_) => {
                // Retry once before failing so a brief DB blip does not lose a freshly rotated token.
                match lease
                    .commit_refresh(refreshing_version, row.nonce.clone(), upsert)
                    .await
                {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::error!(
                            profile = %profile.0,
                            credential = %name.as_str(),
                            code = "credential_commit_failed",
                            "failed to commit refreshed credential after upstream token rotation"
                        );
                        return Err(AuthError::from(err));
                    }
                }
            }
        };
        Self::to_cached(next_version, &next_payload)
    }
}
