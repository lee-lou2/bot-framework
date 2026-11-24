//! HTTP 어댑터. 요청 순서는 다른 입구와 같다: 인증(등록한 발급자·대상·만료) → 정규화 → `Ingress::admit` → 응답.
//!
//! - `POST|PUT|PATCH|DELETE|GET /{endpoint}{path}` — 트리거 `{http: {method, path}, binding}`. `Idempotency-Key`가 필수다.
//!   `accepted`(기본)는 커밋 뒤 202 + 요청 상태 URL, `sync`는 도구 결과 `{status, body}`를 그대로 응답하고
//!   기한을 넘기면 202로 바꾼다.
//! - `GET /{endpoint}/_requests/{id}` — 같은 인증 주체의 요청 결과 재조회(새 도구 호출을 만들지 않는다).
//!   응답 방식은 호출자가 고르지 않는다. 최초 접수 때 저장소에 저장한 방식(`RequestState.http.response`)만 쓰므로
//!   POST 재전달·재조회가 최초 응답과 같은 렌더러를 쓴다(저장된 방식이 없는 이전 요청은 `accepted` 형식).
//!   결과·상태를 돌려주기 직전마다 지금의 바인딩·멤버 권한과 이 프로세스의 판본이 저장소의 활성 판본인지를 다시
//!   확인한다(철회된 뒤에는 본문이 나가지 않고, 구판 프로세스는 503으로 닫는다).
//!
//! 인증 실패는 이유를 알리지 않는다. 등록되지 않은 입구 별칭은 인증 전에 404지만, 등록된 입구는 경로가 없어도 먼저 인증한다.
pub mod reply;
pub mod token;
mod wire;

use crate::{
    active::Release,
    entrance::{Owner, Real},
    host::HttpEndpoint,
    identity,
    inbound::{Event, Inbound, Respond, valid_request_id},
    ingress::{Admission, Ingress},
    limits::RateLimiter,
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
    id::{PrincipalId, RequestId, SecretName},
    intake::{RequestKey, RequestState},
    port::{RunStore, SecretStore},
    state::{Conversation, ConversationKind},
};
use reply::{error, json_response, retry_after};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
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

const KEY_TTL: Duration = Duration::from_secs(30);
const MAX_IN_FLIGHT: usize = 256;

type Clock = dyn Fn() -> u64 + Send + Sync;
/// (프로필, 바인딩, 판본, 자격 이름) — 입구 소유자·판본이 바뀌면 옛 키를 쓰지 않는다.
type KeyId = (String, String, String, String);
type KeyCache = Mutex<HashMap<KeyId, (Instant, Arc<Vec<u8>>)>>;

pub struct HttpGateway {
    ingress: Arc<Ingress>,
    store: Arc<dyn RunStore>,
    secrets: Arc<dyn SecretStore>,
    clock: Box<Clock>,
    keys: KeyCache,
    /// 상태 조회는 제출보다 자주 한다. 제출 한도의 10배까지 허용한다.
    status_limiter: RateLimiter,
    in_flight: Arc<Semaphore>,
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
        }
    }

    /// 시험용: 토큰 만료 판정에 쓰는 시계(유닉스 초)를 바꾼다.
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new().fallback(handle).with_state(self)
    }

    /// stop이 오면 진행 중인 요청을 마치고 돌아온다. TLS 종단·연결 한도·느린 클라이언트 방어는 앞단 프록시 몫이다.
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        stop: CancellationToken,
    ) -> std::io::Result<()> {
        axum::serve(listener, self.router())
            .with_graceful_shutdown(async move { stop.cancelled().await })
            .await
    }
}

async fn handle(State(gateway): State<Arc<HttpGateway>>, request: Request) -> Response {
    let Ok(_permit) = gateway.in_flight.clone().try_acquire_owned() else {
        return unavailable();
    };
    gateway.process(request).await
}

/// 요청 하나가 끝까지 쓰는 입구 문맥. 인증과 접수가 같은 라우팅 표·소유자·판본을 보게 해, 그 사이에 입구 소유자가
/// 바뀌어도 인증한 프로필과 다른 프로필로 접수되지 않는다.
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

    /// 응답(결과·상태) 직전 재확인. 막아야 하면 돌려줄 응답을 준다: 지금의 활성 집합에서 이 입구의 같은 바인딩 멤버가
    /// 아니면 403, 이 프로세스의 판본이 저장소의 활성 판본이 아니면(구판) 503이다.
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
            tracing::debug!(endpoint = ctx.endpoint, ?reason, "토큰을 거절했어요");
            AuthError::Unauthorized
        })
    }

    /// 입구 키(HS256). 값은 바인딩 소유 프로필의 이름 공간에서 읽고, (프로필·바인딩·판본·자격 이름)별로 잠깐만 기억한다.
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
                    "HTTP 입구 키가 {}바이트보다 짧아요",
                    token::MIN_KEY_BYTES
                );
                Err(AuthError::Unavailable)
            }
            None => {
                tracing::error!(endpoint, "HTTP 입구 키를 읽지 못했어요");
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
        // 인증에 쓴 라우팅 표로 접수한다(그 사이 입구 소유자·판본이 바뀌어도 같은 프로필·바인딩으로만 접수된다)
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
                // 최초 접수 때 저장한 응답 방식만 쓴다 — 같은 요청의 재전달도 최초 방식으로 렌더링·대기한다
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
                // 기다리는 동안 권한이 철회됐을 수 있다 — 결과를 돌려주기 직전에 다시 확인한다
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
            // 민감 입력은 DM 전용이라 HTTP 입구에는 올 수 없다. 오면 설정 오류이므로 알리지 않고 거절한다.
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
        // 읽는 사이에 권한이 철회됐을 수 있다 — 상태·결과를 돌려주기 직전에 다시 확인한다
        if let Some(response) = self.withheld(ctx, &principal).await {
            return response;
        }
        match found {
            Ok(None) => not_found(),
            // 접수 때 저장한 응답 방식만 쓴다(호출자가 고를 수 없다)
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

    /// 끝날 때까지(또는 기한까지) 상태를 지켜본다. 새 도구 호출을 만들지 않는다.
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
