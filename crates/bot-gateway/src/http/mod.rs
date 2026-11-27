//! HTTP webhook and OpenAI-compatible ingress adapter.
//!
//! Webhook routes authenticate before returning path 404, replay/poll using only the stored response mode from
//! initial acceptance, and re-verify current binding membership plus active revision immediately before releasing
//! any status or result body.
mod openai;
pub use openai::{Passed, Probe};
mod reply;
pub mod token;
mod wire;

use crate::{
    active::Release,
    entrance::{Owner, Real},
    host::HttpEndpoint,
    identity,
    inbound::{Event, Inbound, Respond, valid_request_id},
    ingress::{Admission, Ingress},
    limits::{RateLimiter, SourceFailures},
    routes::{Routes, STATUS_SEGMENT},
};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use bot_core::{
    api::ApiRuns,
    id::{PrincipalId, RequestId, SecretName},
    intake::{RequestKey, RequestState},
    port::{RunStore, SecretStore},
    profile::ProfileBundles,
    state::{Conversation, ConversationKind},
};
use reply::{error, json_response, retry_after};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_util::sync::CancellationToken;
use wire::bearer;
use wire::{
    AuthError, accepted, finished, forbidden, is_json, not_found, read_body, rejected,
    split_endpoint, state_name, stored_mode, terminal, unauthorized, unavailable,
};

pub const KEEP_ALIVE_DEFAULT: Duration = openai::KEEP_ALIVE_EVERY;
const KEY_TTL: Duration = Duration::from_secs(30);
const MAX_IN_FLIGHT: usize = 256;

type Clock = dyn Fn() -> u64 + Send + Sync;
/// Keyed by `(profile, binding, revision, secret_name)` so ownership or revision changes never reuse old keys.
type KeyId = (String, String, String, String);
type KeyCache = Mutex<HashMap<KeyId, (Instant, Arc<Vec<u8>>)>>;

pub struct HttpGateway {
    ingress: Arc<Ingress>,
    store: Arc<dyn RunStore>,
    secrets: Arc<dyn SecretStore>,
    clock: Box<Clock>,
    keys: KeyCache,
    /// Status polling allows 10x the per-principal submission rate.
    status_limiter: RateLimiter,
    in_flight: Arc<Semaphore>,
    api: Option<Arc<dyn ApiRuns>>,
    bundles: Option<Arc<dyn ProfileBundles>>,
    keep_alive: Duration,
    models_budget: Duration,
    probe: Option<Probe>,
    /// Per-peer-IP auth failure budget rejecting abusive callers before reading SecretStore.
    auth_failures: SourceFailures,
}

impl HttpGateway {
    pub fn new(
        ingress: Arc<Ingress>,
        store: Arc<dyn RunStore>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        let status_per_min = ingress.limits().per_principal_per_min.saturating_mul(10);
        Self {
            ingress,
            store,
            secrets,
            clock: Box::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
            }),
            keys: Mutex::new(HashMap::new()),
            status_limiter: RateLimiter::new(status_per_min),
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            api: None,
            bundles: None,
            keep_alive: openai::KEEP_ALIVE_EVERY,
            models_budget: openai::MODELS_BUDGET,
            probe: None,
            auth_failures: SourceFailures::default(),
        }
    }

    pub fn with_auth_failure_limit(
        mut self,
        max: usize,
        window: Duration,
        block: Duration,
    ) -> Self {
        self.auth_failures = SourceFailures::new(max, window, block);
        self
    }

    pub fn with_probe(mut self, probe: Probe) -> Self {
        self.probe = Some(probe);
        self
    }

    pub fn with_models_budget(mut self, budget: Duration) -> Self {
        self.models_budget = budget;
        self
    }

    pub fn with_connection_limit(mut self, limit: usize) -> Self {
        self.in_flight = Arc::new(Semaphore::new(limit));
        self
    }

    pub fn with_keep_alive(mut self, every: Duration) -> Self {
        self.keep_alive = every;
        self
    }

    pub fn with_openai(mut self, api: Arc<dyn ApiRuns>, bundles: Arc<dyn ProfileBundles>) -> Self {
        self.api = Some(api);
        self.bundles = Some(bundles);
        self
    }

    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    pub fn router(self: Arc<Self>) -> Router {
        self.router_with(CancellationToken::new())
    }

    pub fn router_with(self: Arc<Self>, stop: CancellationToken) -> Router {
        Router::new().fallback(handle).with_state(Shared {
            gateway: self,
            stop,
        })
    }

    /// Serves until `stop` is cancelled; TLS termination and slow-client limits belong to the front proxy.
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        stop: CancellationToken,
    ) -> std::io::Result<()> {
        axum::serve(
            listener,
            self.router_with(stop.clone())
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled().await })
        .await
    }
}

#[derive(Clone)]
struct Shared {
    gateway: Arc<HttpGateway>,
    stop: CancellationToken,
}

async fn handle(State(shared): State<Shared>, request: Request) -> Response {
    let gateway = shared.gateway;
    let openai = openai::is_openai_path(request.uri().path())
        && !gateway.ingress.host().http.openai.is_empty();
    let Ok(permit) = gateway.in_flight.clone().try_acquire_owned() else {
        return if openai {
            openai::busy()
        } else {
            unavailable()
        };
    };
    if openai {
        return gateway.openai(request, permit, shared.stop).await;
    }
    gateway.process(request).await
}

/// Pins routes, owner, and revision for one request so authentication and admission never split across reloads.
struct Ctx<'a> {
    endpoint: &'a str,
    config: &'a HttpEndpoint,
    owner: Owner,
    revision: String,
    routes: Arc<Routes>,
}

impl HttpGateway {
    async fn process(&self, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let Some((endpoint, rest)) = split_endpoint(parts.uri.path()) else {
            return not_found();
        };
        let host = self.ingress.host().clone();
        let Some(config) = host.http.endpoints.get(endpoint) else {
            return not_found();
        };
        let Ok(routes) = self.ingress.routes() else {
            return unavailable();
        };
        let Some(owner) = routes.http_owner(endpoint).cloned() else {
            return not_found();
        };
        let Some(snapshot) = routes.snapshot(&owner.profile) else {
            return not_found();
        };
        let revision = snapshot.revision.0.clone();
        let ctx = Ctx {
            endpoint,
            config,
            owner,
            revision,
            routes,
        };
        let method = parts.method.as_str();
        if !matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
        }
        let subject = match self.authenticate(&ctx, &parts.headers).await {
            Ok(subject) => subject,
            Err(failure) => return failure.response(),
        };
        let Some(principal) = identity::http(endpoint, &subject) else {
            return unauthorized();
        };
        let status_prefix = format!("/{STATUS_SEGMENT}/");
        match rest.strip_prefix(&status_prefix) {
            Some(id) => self.status(&ctx, principal, id, method).await,
            None => {
                self.submit(&ctx, principal, method, rest, parts.headers, body)
                    .await
            }
        }
    }

    async fn withheld(&self, ctx: &Ctx<'_>, principal: &PrincipalId) -> Option<Response> {
        let real = Real::Http {
            endpoint: ctx.endpoint.to_owned(),
        };
        match self.ingress.release(&real, &ctx.owner, principal).await {
            Release::Allowed => None,
            Release::Forbidden => Some(forbidden()),
            Release::Unavailable => Some(unavailable()),
        }
    }

    async fn authenticate(&self, ctx: &Ctx<'_>, headers: &HeaderMap) -> Result<String, AuthError> {
        let token = bearer(headers).ok_or(AuthError::Unauthorized)?;
        let key = self.key(ctx).await?;
        let expect = token::Expect {
            issuer: &ctx.config.issuer,
            audience: &ctx.config.audience,
            max_age_secs: ctx.config.max_token_age_secs,
        };
        token::verify(token, &key, &expect, (self.clock)()).map_err(|reason| {
            tracing::debug!(endpoint = ctx.endpoint, ?reason, "rejected token");
            AuthError::Unauthorized
        })
    }

    async fn key(&self, ctx: &Ctx<'_>) -> Result<Arc<Vec<u8>>, AuthError> {
        let id: KeyId = (
            ctx.owner.profile.0.clone(),
            ctx.owner.binding.0.clone(),
            ctx.revision.clone(),
            ctx.config.key.clone(),
        );
        if let Some((at, key)) = self.keys.lock().unwrap_or_else(|e| e.into_inner()).get(&id)
            && at.elapsed() < KEY_TTL
        {
            return Ok(key.clone());
        }
        let fetched = async {
            let name = SecretName::parse(&ctx.config.key)?;
            let handle = self
                .secrets
                .for_profile(ctx.owner.profile.clone())
                .await
                .ok()?;
            handle.resolve(name).await.ok()
        }
        .await;
        let endpoint = ctx.endpoint;
        match fetched {
            Some(value) if value.0.len() >= token::MIN_KEY_BYTES => {
                let key = Arc::new(value.0);
                let mut cache = self.keys.lock().unwrap_or_else(|e| e.into_inner());
                cache.retain(|_, (at, _)| at.elapsed() < KEY_TTL);
                cache.insert(id, (Instant::now(), key.clone()));
                Ok(key)
            }
            Some(_) => {
                tracing::error!(
                    endpoint,
                    "HTTP endpoint key is shorter than {} bytes",
                    token::MIN_KEY_BYTES
                );
                Err(AuthError::Unavailable)
            }
            None => {
                tracing::error!(endpoint, "failed to read HTTP endpoint key");
                Err(AuthError::Unavailable)
            }
        }
    }

    async fn submit(
        &self,
        ctx: &Ctx<'_>,
        principal: PrincipalId,
        method: &str,
        path: &str,
        headers: HeaderMap,
        body: Body,
    ) -> Response {
        let request = match headers
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .filter(|v| valid_request_id(v))
        {
            Some(key) => key.to_owned(),
            None => return error(StatusCode::BAD_REQUEST, "idempotency_key_required"),
        };
        let bytes = match read_body(body, self.ingress.limits().body_bytes, &headers).await {
            Ok(bytes) => bytes,
            Err(failure) => return failure.response(),
        };
        let value = if bytes.is_empty() {
            Value::Null
        } else if !is_json(&headers) {
            return error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
        } else {
            match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_json"),
            }
        };
        let key = RequestKey {
            profile: ctx.owner.profile.clone(),
            binding: ctx.owner.binding.clone(),
            principal: principal.clone(),
            request: RequestId(request.clone()),
        };
        let admission = self
            .ingress
            .admit_with(
                &ctx.routes,
                Inbound {
                    real: Real::Http {
                        endpoint: ctx.endpoint.to_owned(),
                    },
                    principal: principal.clone(),
                    request: RequestId(request.clone()),
                    conversation: Conversation {
                        kind: ConversationKind::Http,
                        id: ctx.endpoint.to_owned(),
                        thread: Some(request.clone()),
                    },
                    attachments: Vec::new(),
                    body: value,
                    event: Event::Http {
                        method: method.to_owned(),
                        path: path.to_owned(),
                    },
                },
            )
            .await;
        match admission {
            Admission::Accepted { .. } => {
                // Honor the response mode stored on initial acceptance so replays render identically.
                let stored = match self.store.status(key.clone()).await {
                    Ok(stored) => stored,
                    Err(_) => return unavailable(),
                };
                if stored_mode(stored.as_ref()) != Respond::Sync {
                    return accepted(ctx.endpoint, &request);
                }
                let wait = Duration::from_millis(
                    ctx.config
                        .sync_wait_ms
                        .unwrap_or(crate::host::DEFAULT_SYNC_WAIT_MS),
                );
                let waited = self.wait(key, wait).await;
                if let Some(response) = self.withheld(ctx, &principal).await {
                    return response;
                }
                match waited {
                    Waited::Done(state) => finished(&state, &request, Respond::Sync),
                    Waited::Pending => accepted(ctx.endpoint, &request),
                    Waited::Unavailable => unavailable(),
                }
            }
            Admission::Rejected(rejection) => rejected(rejection),
            // Private triggers are DM-only; reject silently if ever reached on HTTP.
            Admission::Private | Admission::Ignored => not_found(),
        }
    }

    async fn status(
        &self,
        ctx: &Ctx<'_>,
        principal: PrincipalId,
        id: &str,
        method: &str,
    ) -> Response {
        if method != "GET" {
            return error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
        }
        if !valid_request_id(id) {
            return not_found();
        }
        if let Some(response) = self.withheld(ctx, &principal).await {
            return response;
        }
        if let Err(wait) = self
            .status_limiter
            .check(&format!("{}/{}", ctx.owner.profile.0, principal.0))
        {
            return retry_after(error(StatusCode::TOO_MANY_REQUESTS, "rate_limited"), wait);
        }
        let key = RequestKey {
            profile: ctx.owner.profile.clone(),
            binding: ctx.owner.binding.clone(),
            principal: principal.clone(),
            request: RequestId(id.to_owned()),
        };
        let found = self.store.status(key).await;
        if let Some(response) = self.withheld(ctx, &principal).await {
            return response;
        }
        match found {
            Ok(None) => not_found(),
            Ok(Some(state)) if terminal(state.state) => {
                finished(&state, id, stored_mode(Some(&state)))
            }
            Ok(Some(state)) => json_response(
                StatusCode::ACCEPTED,
                &json!({"request_id": id, "state": state_name(state.state)}),
            ),
            Err(_) => unavailable(),
        }
    }

    async fn wait(&self, key: RequestKey, wait: Duration) -> Waited {
        let deadline = Instant::now() + wait;
        let mut delay = Duration::from_millis(20);
        loop {
            match self.store.status(key.clone()).await {
                Ok(Some(state)) if terminal(state.state) => return Waited::Done(state),
                Ok(_) => {}
                Err(_) => return Waited::Unavailable,
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Waited::Pending;
            }
            tokio::time::sleep(delay.min(remaining)).await;
            delay = (delay * 2).min(Duration::from_millis(250));
        }
    }
}

enum Waited {
    Done(RequestState),
    Pending,
    Unavailable,
}
