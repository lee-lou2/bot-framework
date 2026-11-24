//! 응답 공통: 보안 헤더, 고정 오류 분류, 크기 상한, 포트 오류의 HTTP 대응.

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bot_core::port::Error;
use serde_json::{Value, json};
use std::sync::Arc;

/// 응답 상한(넘으면 거절한다).
pub const MAX_BODY: usize = 512 * 1024;

/// 응답에 비밀 값이 섞였는지 검사한다(참이면 통과).
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

/// 오류 본문은 고정 분류 낱말 하나뿐이다(내부 사정·입력 원문 금지).
pub fn reply(status: StatusCode, code: &str) -> Response {
    let mut r = (status, axum::Json(json!({"error": code}))).into_response();
    harden(&mut r);
    r
}

/// 포트 오류 → (상태, 고정 낱말).
pub fn classify(e: &Error) -> (StatusCode, &'static str) {
    match e {
        Error::Invalid => (StatusCode::BAD_REQUEST, "invalid"),
        Error::Denied => (StatusCode::FORBIDDEN, "denied"),
        Error::Conflict => (StatusCode::CONFLICT, "conflict"),
        Error::Stale => (StatusCode::CONFLICT, "stale"),
        Error::Missing => (StatusCode::NOT_FOUND, "not_found"),
        Error::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    }
}

pub fn from_error(e: &Error) -> Response {
    let (status, word) = classify(e);
    reply(status, word)
}

/// 작은 오류 값: 상태와 고정 낱말. 응답은 경계에서 만든다(`Result` 오류로 크게 들고 다니지 않는다).
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

/// 200 JSON. 비밀 검사에 걸리거나 상한을 넘으면 내보내지 않는다.
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
