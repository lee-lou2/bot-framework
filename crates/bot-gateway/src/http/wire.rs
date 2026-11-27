//! Webhook HTTP request reading and status/response wire helpers.
use super::reply::{self, error, json_response, retry_after, tool_result, with_header};
use crate::{inbound::Respond, ingress::Rejection, routes::STATUS_SEGMENT};
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use bot_core::{intake::RequestState, state::RunState};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

const BODY_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn unavailable() -> Response {
    retry_after(
        error(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        Duration::from_secs(1),
    )
}

pub(super) fn unauthorized() -> Response {
    with_header(
        error(StatusCode::UNAUTHORIZED, "unauthorized"),
        header::WWW_AUTHENTICATE,
        "Bearer",
    )
}

pub(super) fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found")
}

/// Splits `/{endpoint}{rest}` where `rest` always starts with `/`.
pub(super) fn split_endpoint(path: &str) -> Option<(&str, &str)> {
    let trimmed = path.strip_prefix('/')?;
    let (endpoint, rest) = match trimmed.find('/') {
        Some(at) => (&trimmed[..at], &trimmed[at..]),
        None => (trimmed, "/"),
    };
    (!endpoint.is_empty()).then_some((endpoint, rest))
}

pub(super) fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && !token.contains(' '))
        .then_some(token)
}

pub(super) fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
}

pub(super) fn terminal(state: RunState) -> bool {
    matches!(
        state,
        RunState::Succeeded | RunState::Dead | RunState::Cancelled
    )
}

pub(super) fn state_name(state: RunState) -> &'static str {
    match state {
        RunState::Waiting => "waiting",
        _ => "pending",
    }
}

pub(super) fn rejected(rejection: Rejection) -> Response {
    match rejection {
        Rejection::Unregistered | Rejection::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        Rejection::RateLimited(wait) => {
            retry_after(error(StatusCode::TOO_MANY_REQUESTS, "rate_limited"), wait)
        }
        Rejection::TooLarge => error(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
        Rejection::NoRoute => not_found(),
        Rejection::Invalid => error(StatusCode::BAD_REQUEST, "invalid_request"),
        Rejection::Conflict => error(StatusCode::CONFLICT, "conflict"),
        Rejection::Unavailable => unavailable(),
    }
}

pub(super) enum BodyError {
    TooLarge,
    Invalid,
    Timeout,
}

impl BodyError {
    pub(super) fn response(self) -> Response {
        match self {
            BodyError::TooLarge => error(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
            BodyError::Invalid => error(StatusCode::BAD_REQUEST, "invalid_request"),
            BodyError::Timeout => error(StatusCode::REQUEST_TIMEOUT, "request_timeout"),
        }
    }
}

pub(super) enum AuthError {
    Unauthorized,
    Unavailable,
}

impl AuthError {
    pub(super) fn response(self) -> Response {
        match self {
            AuthError::Unauthorized => unauthorized(),
            AuthError::Unavailable => unavailable(),
        }
    }
}

pub(super) async fn read_body(
    body: Body,
    limit: usize,
    headers: &HeaderMap,
) -> Result<Vec<u8>, BodyError> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|len| len > limit) {
        return Err(BodyError::TooLarge);
    }
    let read = async {
        let mut stream = body.into_data_stream();
        let mut data = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| BodyError::Invalid)?;
            if data.len() + chunk.len() > limit {
                return Err(BodyError::TooLarge);
            }
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    };
    tokio::time::timeout(BODY_TIMEOUT, read)
        .await
        .unwrap_or(Err(BodyError::Timeout))
}

pub(super) fn accepted(endpoint: &str, request: &str) -> Response {
    let location = format!("/{endpoint}/{STATUS_SEGMENT}/{request}");
    with_header(
        json_response(
            StatusCode::ACCEPTED,
            &json!({"request_id": request, "state": "pending"}),
        ),
        header::LOCATION,
        &location,
    )
}

/// Renders a finished request identically for initial and polled responses according to the stored `mode`.
/// Invalid `{status, ...}` attempts always fail with 502; only `Accepted` wraps non-tool JSON results.
pub(super) fn finished(state: &RequestState, request: &str, mode: Respond) -> Response {
    let lenient = mode == Respond::Accepted;
    match state.state {
        RunState::Succeeded => {
            let Some(result) = &state.result else {
                return error(StatusCode::BAD_GATEWAY, "invalid_result");
            };
            if let Some(response) = tool_result(result) {
                return response;
            }
            let wrapped = match serde_json::from_str::<Value>(&result.0) {
                Ok(raw) if lenient && result.0.len() <= reply::RESULT_LIMIT => {
                    let attempted = raw.as_object().is_some_and(|o| o.contains_key("status"));
                    (!attempted).then_some(raw)
                }
                _ => None,
            };
            match wrapped {
                Some(raw) => json_response(
                    StatusCode::OK,
                    &json!({"request_id": request, "state": "succeeded", "result": raw}),
                ),
                None => error(StatusCode::BAD_GATEWAY, "invalid_result"),
            }
        }
        RunState::Cancelled => json_response(
            StatusCode::CONFLICT,
            &json!({"request_id": request, "error": "cancelled"}),
        ),
        _ => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"request_id": request, "error": "failed"}),
        ),
    }
}

pub(super) fn forbidden() -> Response {
    error(StatusCode::FORBIDDEN, "forbidden")
}

/// Defaults legacy rows lacking stored HTTP route metadata conservatively to `Accepted`.
pub(super) fn stored_mode(state: Option<&RequestState>) -> Respond {
    state
        .and_then(|s| s.http.as_ref())
        .map_or(Respond::Accepted, |route| Respond::from(&route.response))
}
