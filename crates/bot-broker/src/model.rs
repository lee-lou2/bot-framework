//! Model provider proxy enforcing host-side secret isolation, alias rewriting, and leak checks.
//!
//! Sandboxes have neither keys nor direct network access. Provider keys are resolved here and
//! attached only to upstream headers; responses (and SSE streams via `stream`) are scanned for
//! leaked credentials before returning. Every request checks run/generation/claim in the ledger
//! and pins the upstream model name from the operator's alias map.

mod stream;
mod vet;

pub(crate) use stream::error_event;
pub use stream::{ChunkSource, ModelStream, UpstreamStream};
use vet::vet_request;

use crate::{
    canon::canonicalize,
    guard::contains_credential,
    json::parse,
    rate::{Limit, Limiter, RateConfig, Slot},
    resolve::HostPort,
    sys::Clock,
};
use bot_core::{
    command::JsonPayload,
    id::*,
    ledger::CallLedger,
    port::{Error, Reply, SecretStore},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::time::timeout;

#[derive(Clone, Debug)]
pub struct Provider {
    pub endpoint: HostPort,
    pub path: String,
    /// Profile-scoped secret name attached only to upstream request headers.
    pub secret: SecretName,
    /// Header name (e.g. `authorization`, `x-api-key`) and optional value prefix (e.g. `Bearer`).
    pub header: String,
    pub scheme: Option<String>,
    /// Fixed non-secret upstream headers (e.g. API version).
    pub headers: Vec<(String, String)>,
    /// Lowercase sandbox header names permitted to pass through (all others are dropped).
    pub forward_headers: Vec<String>,
    /// Model alias -> upstream model ID.
    pub models: BTreeMap<String, String>,
    /// Additional operator-permitted top-level request body fields beyond the base allowlist.
    pub body_fields: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ModelHost {
    pub providers: BTreeMap<String, Provider>,
    /// Profile -> operator-allowed provider names.
    pub allow: BTreeMap<ProfileId, BTreeSet<String>>,
}

#[derive(Clone, Copy, Debug)]
pub struct ModelLimits {
    pub max_request: usize,
    pub max_response: usize,
    pub timeout: Duration,
    pub rate: RateConfig,
    pub stream_idle: Duration,
    pub stream_total: Duration,
    pub max_event: usize,
    pub max_stream: usize,
}

impl Default for ModelLimits {
    fn default() -> Self {
        Self {
            max_request: 4 << 20,
            max_response: 8 << 20,
            timeout: Duration::from_secs(120),
            rate: RateConfig {
                per_minute: 120,
                burst: 30,
                concurrent: 4,
            },
            stream_idle: Duration::from_secs(60),
            stream_total: Duration::from_secs(900),
            max_event: 1 << 20,
            max_stream: 32 << 20,
        }
    }
}

/// Model request from the sandbox; `alias` is pinned to the session by the host.
pub struct ModelRequest {
    pub attempt: Attempt,
    pub alias: String,
    pub body: JsonPayload,
    pub headers: Vec<(String, String)>,
}

pub struct ModelResponse {
    pub status: u16,
    pub body: JsonPayload,
}

pub struct UpstreamRequest {
    pub endpoint: HostPort,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct UpstreamResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Upstream transport port; implementations enforce TLS and perform no hidden retries.
pub trait Upstream: Send + Sync {
    fn send(&self, request: UpstreamRequest) -> Reply<'_, UpstreamResponse>;
    /// Streams response chunks (SSE) once response headers arrive.
    fn stream(&self, request: UpstreamRequest) -> Reply<'_, UpstreamStream>;
}

pub struct ModelProxy {
    host: ModelHost,
    ledger: Arc<dyn CallLedger>,
    secrets: Arc<dyn SecretStore>,
    upstream: Arc<dyn Upstream>,
    limiter: Arc<Limiter>,
    limits: ModelLimits,
}

/// Holds the concurrency slot until the upstream call finishes.
struct Prepared {
    slot: Slot,
    request: UpstreamRequest,
    key: Vec<u8>,
}

/// Printable ASCII only, up to 512 bytes.
fn header_value_ok(value: &str) -> bool {
    value.len() <= 512 && value.bytes().all(|b| (0x20..0x7f).contains(&b))
}

impl ModelProxy {
    pub fn new(
        host: ModelHost,
        ledger: Arc<dyn CallLedger>,
        secrets: Arc<dyn SecretStore>,
        upstream: Arc<dyn Upstream>,
        clock: Arc<dyn Clock>,
        limits: ModelLimits,
    ) -> Self {
        Self {
            host,
            ledger,
            secrets,
            upstream,
            limiter: Limiter::new(limits.rate, clock),
            limits,
        }
    }

    /// Resolves a unique allowed provider and upstream model for `(profile, alias)`; returns `None` if missing or ambiguous.
    fn provider(&self, profile: &ProfileId, alias: &str) -> Option<(&Provider, &String)> {
        let allowed = self.host.allow.get(profile)?;
        let mut found = allowed
            .iter()
            .filter_map(|name| self.host.providers.get(name))
            .filter_map(|p| p.models.get(alias).map(|model| (p, model)));
        let first = found.next()?;
        found.next().is_none().then_some(first)
    }

    /// Verifies that `attempt` still holds the active claim in the ledger.
    pub async fn verify(&self, attempt: Attempt) -> Result<bot_core::state::RunContext, Error> {
        self.ledger.context(attempt).await
    }

    async fn prepare(&self, req: ModelRequest, streaming: bool) -> Result<Prepared, Error> {
        let profile = req.attempt.profile.clone();
        let ctx = self.ledger.context(req.attempt).await?;
        let (provider, model) = self.provider(&profile, &req.alias).ok_or(Error::Denied)?;
        let slot = self
            .limiter
            .admit(&profile, &ctx.principal)
            .map_err(|limit| match limit {
                Limit::Rate | Limit::Concurrency => Error::Unavailable,
            })?;
        let body = self.request_body(&req.body, model, streaming, &provider.body_fields)?;
        let key = self
            .secrets
            .for_profile(profile)
            .await?
            .resolve(provider.secret.clone())
            .await
            .map_err(|_| Error::Unavailable)?
            .0;
        let secret = String::from_utf8(key.clone()).map_err(|_| Error::Unavailable)?;
        let mut headers = vec![
            (
                provider.header.clone(),
                match &provider.scheme {
                    Some(scheme) => format!("{scheme} {secret}"),
                    None => secret,
                },
            ),
            ("content-type".to_owned(), "application/json".to_owned()),
        ];
        if streaming {
            headers.push(("accept".to_owned(), "text/event-stream".to_owned()));
        }
        headers.extend(provider.headers.iter().cloned());
        for (name, value) in &req.headers {
            let name = name.to_ascii_lowercase();
            let fixed = provider
                .headers
                .iter()
                .any(|(fixed, _)| fixed.eq_ignore_ascii_case(&name));
            if provider.forward_headers.contains(&name) && !fixed && header_value_ok(value) {
                headers.push((name, value.clone()));
            }
        }
        Ok(Prepared {
            slot,
            request: UpstreamRequest {
                endpoint: provider.endpoint.clone(),
                path: provider.path.clone(),
                headers,
                body: body.into_bytes(),
            },
            key,
        })
    }

    pub async fn forward(&self, req: ModelRequest) -> Result<ModelResponse, Error> {
        let Prepared {
            slot: _slot,
            request,
            key,
        } = self.prepare(req, false).await?;
        let response = timeout(self.limits.timeout, self.upstream.send(request))
            .await
            .map_err(|_| Error::Unavailable)??;
        self.response_body(response, &key)
    }

    /// Relays a streaming (`stream: true`) request; upstream errors collapse to a fixed status payload.
    pub async fn forward_stream(&self, req: ModelRequest) -> Result<ModelStream, Error> {
        let Prepared { slot, request, key } = self.prepare(req, true).await?;
        let upstream = timeout(self.limits.timeout, self.upstream.stream(request))
            .await
            .map_err(|_| Error::Unavailable)??;
        Ok(ModelStream::new(upstream, key, self.limits, slot))
    }

    /// Parses strictly, validates fields, pins the upstream model, enforces streaming mode, and canonicalizes.
    fn request_body(
        &self,
        body: &JsonPayload,
        model: &str,
        streaming: bool,
        extra: &[String],
    ) -> Result<String, Error> {
        if body.0.len() > self.limits.max_request {
            return Err(Error::Invalid);
        }
        let Some(Value::Object(mut map)) = parse(&body.0) else {
            return Err(Error::Invalid);
        };
        let wants_stream = match map.get("stream") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(_) => return Err(Error::Invalid),
        };
        if wants_stream != streaming {
            return Err(Error::Invalid);
        }
        vet_request(&map, extra)?;
        map.insert("model".into(), Value::String(model.to_owned()));
        canonicalize(&Value::Object(map)).ok_or(Error::Invalid)
    }

    /// Checks response size, JSON validity, and credential leaks; strips upstream error bodies to status only.
    fn response_body(
        &self,
        response: UpstreamResponse,
        key: &[u8],
    ) -> Result<ModelResponse, Error> {
        if response.body.len() > self.limits.max_response {
            return Err(Error::Unavailable);
        }
        if contains_credential(&response.body, &[key.to_vec()]) {
            return Err(Error::Denied);
        }
        if !(200..300).contains(&response.status) {
            let status = response.status;
            return Ok(ModelResponse {
                status,
                body: JsonPayload(format!("{{\"error\":{{\"status\":{status}}}}}")),
            });
        }
        let text = String::from_utf8(response.body).map_err(|_| Error::Unavailable)?;
        let decoded = parse(&text).ok_or(Error::Unavailable)?;
        // Scan decoded strings and keys as well so JSON-escaped credentials cannot bypass the raw byte check.
        if decoded_leaks(&decoded, key) {
            return Err(Error::Denied);
        }
        Ok(ModelResponse {
            status: response.status,
            body: JsonPayload(text),
        })
    }
}

/// Returns true if any decoded JSON key or string value contains `key`.
pub(crate) fn decoded_leaks(value: &Value, key: &[u8]) -> bool {
    let hit = |text: &str| contains_credential(text.as_bytes(), &[key.to_vec()]);
    match value {
        Value::String(text) => hit(text),
        Value::Array(items) => items.iter().any(|item| decoded_leaks(item, key)),
        Value::Object(map) => map
            .iter()
            .any(|(name, item)| hit(name) || decoded_leaks(item, key)),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
