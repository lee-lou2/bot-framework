//! OpenAI 호환 HTTP 입구: 루트 `/v1` 아래의 `GET /models`, `GET /models/{id}`, `POST /chat/completions`.
//!
//! - **키가 입구를 정한다.** URL에는 입구 별칭이 없다. `Authorization: Bearer <키>`의 값이 호스트의 어느 OpenAI 입구·키 슬롯에 맞는지로 입구와 주체(`http:<별칭>/<주체>`)가 정해지고,
//!   그 입구를 쓰는 활성 프로필이 권한 범위다. JWT 검증으로 넘어가는 폴백은 없다(JWT는 웹훅 입구 전용).
//! - 경로·메서드를 먼저 가리고(없으면 404, 틀리면 405 + `Allow`) 인증한 뒤 처리한다. 오류는 모두 OpenAI 형식의 고정 문구다(`error.rs`).
//! - 채팅은 매 POST가 서버가 정한 새 HTTP 대화다. 마지막 user 턴이 현재 질문이고 나머지 턴은 순서·역할 그대로 비신뢰 문맥이다(`request.rs`).
//!   멱등 키로 접수·재전달을 한 트랜잭션에서 가리고(`ApiRuns`), 응답은 접수 뒤 단조시계 기한 안에서 기다려 JSON 또는 SSE로 낸다.
//! - 결과·안내를 내보내기 직전마다 키와 권한(멤버·같은 모델 이름이 접수했던 바인딩·에이전트를 가리키는지·활성 집합)을 다시 확인한다.
mod auth;
mod body;
mod chat;
mod digest;
mod error;
mod models;
mod render;
mod request;
mod stream;
mod strict;

pub(super) use models::BUDGET as MODELS_BUDGET;
pub(super) use stream::KEEP_ALIVE_EVERY;
pub use stream::{Passed, Probe};

use super::HttpGateway;
use auth::AuthError;
use axum::{
    extract::{ConnectInfo, Request},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use error::Failure;
use serde_json::Value;
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Instant,
};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

/// 실제 연결의 피어 IP(`ConnectInfo`). IPv4-매핑 IPv6는 IPv4로 바로잡아 같은 출처로 센다. 요청 머리(`X-Forwarded-For` 등)는 속일 수 있어 보지 않는다.
/// 피어 주소를 알 수 없으면 None(그런 경로는 한 칸을 같이 쓴다).
fn peer(extensions: &axum::http::Extensions) -> Option<IpAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip().to_canonical())
}

/// 이 경로는 OpenAI 호환 입구의 몫인가(호스트에 OpenAI 입구가 있을 때 `/v1` 아래 전부)
pub(super) fn is_openai_path(path: &str) -> bool {
    path == "/v1" || path.starts_with("/v1/")
}

/// 동시 연결 자리가 모두 찼을 때의 OpenAI 형식 응답(접수 전이라 SDK가 다시 보내도 된다)
pub(super) fn busy() -> Response {
    stamp(Failure::unavailable().response(), &request_id(), None)
}

/// 요청 하나가 끝까지 들고 가는 것: 운영용 요청 ID, 종료 신호, 동시 연결 자리(스트림이면 스트림이 끝날 때까지 쥔다)
pub(super) struct Cx {
    pub request_id: String,
    pub stop: CancellationToken,
    pub permit: OwnedSemaphorePermit,
}

enum Route<'a> {
    Models,
    Model(&'a str),
    Chat,
}

/// 경로·메서드 판정. 지원하지 않는 경로는 404, 알려진 경로의 다른 메서드는 405(`Allow`)다.
fn route<'a>(method: &str, path: &'a str) -> Result<Route<'a>, Failure> {
    let (route, allow) = match path {
        "/v1/models" => (Route::Models, "GET"),
        "/v1/chat/completions" => (Route::Chat, "POST"),
        other => match other.strip_prefix("/v1/models/") {
            Some(id) if !id.is_empty() && !id.contains('/') => (Route::Model(id), "GET"),
            _ => return Err(Failure::not_found()),
        },
    };
    if method == allow {
        Ok(route)
    } else {
        Err(Failure::method_not_allowed(allow))
    }
}

/// 운영용 불투명 ID(비밀이 아니다).
fn request_id() -> String {
    format!("req_{}", bot_core::random::random_hex(16))
}

/// 모든 OpenAI 응답에 운영 ID를, 멱등 키가 정해졌으면 적용한 키를 단다.
fn stamp(mut response: Response, request_id: &str, key: Option<&str>) -> Response {
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(request_id) {
        headers.insert("x-request-id", value);
    }
    if let Some(value) = key.and_then(|key| HeaderValue::from_str(key).ok()) {
        headers.insert("idempotency-key", value);
    }
    response
}

fn json(value: &Value) -> Response {
    super::reply::json_response(StatusCode::OK, value)
}

impl HttpGateway {
    pub(super) async fn openai(
        self: Arc<Self>,
        request: Request,
        permit: OwnedSemaphorePermit,
        stop: CancellationToken,
    ) -> Response {
        let request_id = request_id();
        let (parts, body) = request.into_parts();
        let route = match route(parts.method.as_str(), parts.uri.path()) {
            Ok(route) => route,
            Err(failure) => return stamp(failure.response(), &request_id, None),
        };
        let Ok(routes) = self.ingress.routes() else {
            return stamp(Failure::unavailable().response(), &request_id, None);
        };
        // 키를 맞춰 보기 전에(비밀 보관소를 읽기 전에) 이 출처가 인증 실패로 막혀 있는지 본다
        let source = peer(&parts.extensions);
        if let Err(wait) = self.auth_failures.check(source, Instant::now()) {
            return stamp(Failure::auth_blocked(wait).response(), &request_id, None);
        }
        let authed = match self.openai_authenticate(&routes, &parts.headers).await {
            Ok(authed) => authed,
            Err(AuthError::Invalid) => {
                // 키를 내밀었는데 맞지 않다: 추측으로 센다(머리가 아예 없는 요청은 추측이 아니다)
                if parts.headers.contains_key(header::AUTHORIZATION) {
                    self.auth_failures.fail(source, Instant::now());
                }
                return stamp(Failure::invalid_api_key().response(), &request_id, None);
            }
            Err(AuthError::Unavailable) => {
                return stamp(Failure::unavailable().response(), &request_id, None);
            }
        };
        let listed = match route {
            Route::Models => Some(None),
            Route::Model(id) => Some(Some(id)),
            Route::Chat => None,
        };
        if let Some(which) = listed {
            let response = match self.models(&authed, which).await {
                Ok(value) => json(&value),
                Err(failure) => failure.response(),
            };
            return stamp(response, &request_id, None);
        }
        let cx = Cx {
            request_id,
            stop,
            permit,
        };
        self.chat(cx, authed, routes, parts, body).await
    }
}

#[cfg(test)]
mod tests;
