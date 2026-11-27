//! OpenAI-compatible HTTP ingress dispatcher under root `/v1`.
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

/// Uses actual TCP peer IP only (ignoring spoofable forwarding headers) and canonicalizes IPv4-mapped IPv6.
fn peer(extensions: &axum::http::Extensions) -> Option<IpAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip().to_canonical())
}

pub(super) fn is_openai_path(path: &str) -> bool {
    path == "/v1" || path.starts_with("/v1/")
}

pub(super) fn busy() -> Response {
    stamp(Failure::unavailable().response(), &request_id(), None)
}

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

fn request_id() -> String {
    format!("req_{}", bot_core::random::random_hex(16))
}

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
        // Check source auth lockout before touching the secret store.
        let source = peer(&parts.extensions);
        if let Err(wait) = self.auth_failures.check(source, Instant::now()) {
            return stamp(Failure::auth_blocked(wait).response(), &request_id, None);
        }
        let authed = match self.openai_authenticate(&routes, &parts.headers).await {
            Ok(authed) => authed,
            Err(AuthError::Invalid) => {
                // Only count presented credentials toward brute-force lockout.
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
