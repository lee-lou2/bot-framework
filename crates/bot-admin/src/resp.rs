//! Shared admin HTTP response helpers: security headers, fixed error codes, size caps, and `Error` mapping.

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bot_core::port::Error;
use serde_json::{Value, json};
use std::sync::Arc;

pub const MAX_BODY: usize = 512 * 1024;

/// Returns true when the response JSON contains no known secret values.
pub type LeakCheck = Arc<dyn Fn(&Value) -> bool + Send + Sync>;

pub fn harden(r: &mut Response) {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}

/// Emits a fixed vocabulary error code without leaking internals or raw input.
pub fn reply(status: StatusCode, code: &str) -> Response {
    let mut r = (status, axum::Json(json!({"error": code}))).into_response();
    harden(&mut r);
    r
}

pub fn classify(e: &Error) -> (StatusCode, &'static str) {
    match e {
        Error::Invalid => (StatusCode::BAD_REQUEST, "invalid"),
        Error::Denied => (StatusCode::FORBIDDEN, "denied"),
        Error::Conflict => (StatusCode::CONFLICT, "conflict"),
        Error::Stale => (StatusCode::CONFLICT, "stale"),
        Error::Missing => (StatusCode::NOT_FOUND, "not_found"),
        Error::Unavailable | Error::RateLimited => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    }
}

#[cfg(test)]
fn from_error(e: &Error) -> Response {
    let (status, word) = classify(e);
    reply(status, word)
}

#[derive(Debug)]
pub struct Fail(pub StatusCode, pub &'static str);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        reply(self.0, self.1)
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        let (status, word) = classify(&e);
        Self(status, word)
    }
}

/// Emits 200 JSON unless the leak check fails or the serialized body exceeds `MAX_BODY`.
pub fn ok(body: &Value, leak: Option<&LeakCheck>) -> Response {
    if leak.is_some_and(|check| !check(body)) {
        return reply(StatusCode::INTERNAL_SERVER_ERROR, "leak");
    }
    match serde_json::to_vec(body) {
        Ok(bytes) if bytes.len() <= MAX_BODY => {
            let mut r = (StatusCode::OK, bytes).into_response();
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            );
            harden(&mut r);
            r
        }
        _ => reply(StatusCode::INTERNAL_SERVER_ERROR, "too_large"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_errors_map_to_fixed_statuses_and_words() {
        let cases = [
            (Error::Invalid, 400, "invalid"),
            (Error::Denied, 403, "denied"),
            (Error::Conflict, 409, "conflict"),
            (Error::Stale, 409, "stale"),
            (Error::Missing, 404, "not_found"),
            (Error::Unavailable, 503, "unavailable"),
        ];
        for (e, status, word) in cases {
            let r = from_error(&e);
            assert_eq!(r.status().as_u16(), status, "{word}");
            assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        }
    }

    #[test]
    fn leaking_or_oversized_bodies_are_not_sent() {
        let leak: LeakCheck = Arc::new(|v| !v.to_string().contains("SECRET"));
        assert_eq!(ok(&json!({"a": 1}), Some(&leak)).status(), StatusCode::OK);
        assert_eq!(
            ok(&json!({"a": "SECRET"}), Some(&leak)).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let big = json!({"a": "x".repeat(MAX_BODY)});
        assert_eq!(ok(&big, None).status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
