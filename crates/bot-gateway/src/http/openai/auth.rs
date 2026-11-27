//! Constant-time Bearer API key authentication across all active OpenAI key slots.
use super::super::HttpGateway;
use crate::{host::openai_key_shaped, identity, routes::Routes};
use axum::http::{HeaderMap, header};
use bot_core::{
    id::{PrincipalId, SecretName},
    port::Error,
};
use futures_util::future::join_all;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::time::timeout;

/// Per-slot secret read timeout so a stalled store cannot pin requests.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

type Hash = [u8; 32];

enum Slot {
    Value(Vec<u8>),
    Absent,
    Unreadable,
}

/// Authenticated caller carrying only the key's SHA-256 hash for pre-delivery re-verification.
pub(super) struct Authed {
    pub endpoint: String,
    pub subject: String,
    pub principal: PrincipalId,
    hash: Hash,
}

impl Authed {
    #[cfg(test)]
    pub(super) fn for_test(endpoint: &str, subject: &str) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            subject: subject.to_owned(),
            principal: identity::http(endpoint, subject).expect("valid HTTP principal"),
            hash: [0; 32],
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum AuthError {
    /// Missing, malformed, unmatched, or ambiguously duplicated key.
    Invalid,
    /// Uniqueness could not be verified due to a store failure or timeout.
    Unavailable,
}

/// Accepts exactly one `Authorization: Bearer <key>` header; rejects duplicates.
fn presented(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.to_str().ok()?.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && openai_key_shaped(token.as_bytes())).then_some(token)
}

/// Constant-time comparison over fixed-length hashes.
fn same(a: &Hash, b: &Hash) -> bool {
    std::hint::black_box(a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y))) == 0
}

impl HttpGateway {
    pub(super) async fn openai_authenticate(
        &self,
        routes: &Routes,
        headers: &HeaderMap,
    ) -> Result<Authed, AuthError> {
        let token = presented(headers).ok_or(AuthError::Invalid)?;
        let hash: Hash = Sha256::digest(token.as_bytes()).into();
        let (endpoint, subject) = self.match_key(routes, hash).await?;
        let principal = identity::http(&endpoint, &subject).ok_or(AuthError::Unavailable)?;
        Ok(Authed {
            endpoint,
            subject,
            principal,
            hash,
        })
    }

    /// Re-verifies right before emitting output that the same key still uniquely identifies the same endpoint and subject.
    pub(super) async fn openai_reauthenticate(&self, authed: &Authed) -> Result<(), AuthError> {
        let routes = self.ingress.routes().map_err(|_| AuthError::Unavailable)?;
        let (endpoint, subject) = self.match_key(&routes, authed.hash).await?;
        if endpoint == authed.endpoint && subject == authed.subject {
            Ok(())
        } else {
            Err(AuthError::Invalid)
        }
    }

    async fn match_key(
        &self,
        routes: &Routes,
        presented: Hash,
    ) -> Result<(String, String), AuthError> {
        let host = self.ingress.host().clone();
        let mut matched = 0usize;
        let mut found = None;
        let mut unverifiable = false;
        // Scan every slot without short-circuiting so duplicate keys fail closed and timing stays uniform.
        for (alias, endpoint) in &host.http.openai {
            let Some(profile) = routes.openai_profile(alias) else {
                continue;
            };
            let handle = match timeout(READ_TIMEOUT, self.secrets.for_profile(profile.clone()))
                .await
            {
                Ok(Ok(handle)) => handle,
                _ => {
                    tracing::error!(endpoint = %alias, "failed to open secret store handle for OpenAI endpoint");
                    unverifiable = true;
                    continue;
                }
            };
            let reads = endpoint.keys.iter().map(|(slot, key)| {
                let handle = &handle;
                async move {
                    let Some(name) = SecretName::parse(&key.secret) else {
                        return (slot, key, Slot::Absent);
                    };
                    match timeout(READ_TIMEOUT, handle.resolve(name)).await {
                        Ok(Ok(value)) => (slot, key, Slot::Value(value.0)),
                        // Missing slots cannot collide with any other slot; only unreadable slots fail uniqueness.
                        Ok(Err(Error::Missing)) => (slot, key, Slot::Absent),
                        _ => (slot, key, Slot::Unreadable),
                    }
                }
            });
            for (slot, key, read) in join_all(reads).await {
                let value = match read {
                    Slot::Value(value) => value,
                    Slot::Absent => {
                        tracing::error!(endpoint = %alias, slot = %slot, "OpenAI key slot missing from secret store; skipping slot");
                        continue;
                    }
                    Slot::Unreadable => {
                        tracing::error!(endpoint = %alias, slot = %slot, "failed to read OpenAI key slot; failing closed");
                        unverifiable = true;
                        continue;
                    }
                };
                // Trim trailing newlines/whitespace from secret files to match redaction handling.
                let secret = value.trim_ascii();
                if !openai_key_shaped(secret) {
                    tracing::error!(endpoint = %alias, slot = %slot, "OpenAI key slot value violates key shape (32-256 printable ASCII bytes); skipping slot");
                    continue;
                }
                let hash: Hash = Sha256::digest(secret).into();
                if same(&hash, &presented) {
                    matched += 1;
                    found = Some((alias.clone(), key.subject.clone()));
                }
            }
        }
        if unverifiable {
            return Err(AuthError::Unavailable);
        }
        match (matched, found) {
            (1, Some(found)) => Ok(found),
            _ => Err(AuthError::Invalid),
        }
    }
}

#[cfg(test)]
mod tests;
