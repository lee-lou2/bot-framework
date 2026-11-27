//! OpenAI error payloads and status mapping with fixed non-echoing messages and retry controls.
use axum::{
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    InvalidRequest,
    Authentication,
    Permission,
    RateLimit,
    Server,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::InvalidRequest => "invalid_request_error",
            Kind::Authentication => "authentication_error",
            Kind::Permission => "permission_error",
            Kind::RateLimit => "rate_limit_error",
            Kind::Server => "server_error",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub status: StatusCode,
    pub kind: Kind,
    pub code: &'static str,
    pub message: &'static str,
    /// Schema field path only (e.g. `messages[0].content`); never contains caller values.
    pub param: Option<String>,
    pub retry_after: Option<Duration>,
    /// Emits `x-should-retry: false` to prevent SDK auto-retries after possible admission.
    pub no_retry: bool,
    pub allow: Option<&'static str>,
    pub challenge: bool,
}

fn failure(status: StatusCode, kind: Kind, code: &'static str, message: &'static str) -> Failure {
    Failure {
        status,
        kind,
        code,
        message,
        param: None,
        retry_after: None,
        no_retry: false,
        allow: None,
        challenge: false,
    }
}

impl Failure {
    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    pub fn invalid(message: &'static str) -> Self {
        failure(
            StatusCode::BAD_REQUEST,
            Kind::InvalidRequest,
            "invalid_request",
            message,
        )
    }

    pub fn unsupported(param: impl Into<String>) -> Self {
        let param = param.into();
        let failure = failure(
            StatusCode::BAD_REQUEST,
            Kind::InvalidRequest,
            "unsupported_parameter",
            "This parameter or value is not supported by this server",
        );
        if param.is_empty() {
            failure
        } else {
            failure.with_param(param)
        }
    }

    pub fn idempotency_conflict() -> Self {
        failure(
            StatusCode::BAD_REQUEST,
            Kind::InvalidRequest,
            "idempotency_key_conflict",
            "A different request already exists for this Idempotency-Key — use a new key for a different request",
        )
    }

    /// Rejects keyless SDK retries (`x-stainless-retry-count > 0`) so they never spawn duplicate runs.
    pub fn idempotency_required() -> Self {
        failure(
            StatusCode::BAD_REQUEST,
            Kind::InvalidRequest,
            "idempotency_key_required",
            "Retried requests require the original Idempotency-Key — no new run was created",
        )
    }

    pub fn invalid_api_key() -> Self {
        Self {
            challenge: true,
            ..failure(
                StatusCode::UNAUTHORIZED,
                Kind::Authentication,
                "invalid_api_key",
                "Missing or invalid API key",
            )
        }
    }

    pub fn access_denied() -> Self {
        failure(
            StatusCode::FORBIDDEN,
            Kind::Permission,
            "access_denied",
            "This key is not authorized to process this request or read its result",
        )
    }

    /// Shared response for missing, unauthorized, or reassigned models so existence is never leaked.
    pub fn model_not_found() -> Self {
        failure(
            StatusCode::NOT_FOUND,
            Kind::InvalidRequest,
            "model_not_found",
            "Model does not exist or is not accessible with this key",
        )
        .with_param("model")
    }

    pub fn not_found() -> Self {
        failure(
            StatusCode::NOT_FOUND,
            Kind::InvalidRequest,
            "not_found",
            "Unsupported endpoint path",
        )
    }

    pub fn method_not_allowed(allow: &'static str) -> Self {
        Self {
            allow: Some(allow),
            ..failure(
                StatusCode::METHOD_NOT_ALLOWED,
                Kind::InvalidRequest,
                "method_not_allowed",
                "HTTP method is not allowed for this path",
            )
        }
    }

    pub fn too_large() -> Self {
        failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            Kind::InvalidRequest,
            "request_too_large",
            "Request body exceeds size limit",
        )
    }

    pub fn unsupported_media() -> Self {
        failure(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Kind::InvalidRequest,
            "unsupported_media_type",
            "Content-Type must be application/json",
        )
    }

    /// Pre-admission read timeout; safe for SDK automatic retry.
    pub fn read_timeout() -> Self {
        failure(
            StatusCode::REQUEST_TIMEOUT,
            Kind::InvalidRequest,
            "request_timeout",
            "Timed out while reading request body",
        )
    }

    /// Pre-admission rate or capacity limit; nothing was recorded.
    pub fn rate_limited(wait: Duration) -> Self {
        Self {
            retry_after: Some(wait),
            ..failure(
                StatusCode::TOO_MANY_REQUESTS,
                Kind::RateLimit,
                "rate_limit_exceeded",
                "Rate limit exceeded — try again shortly",
            )
        }
    }

    /// Source-level auth failure lockout applied before secret store reads.
    pub fn auth_blocked(wait: Duration) -> Self {
        Self {
            retry_after: Some(wait),
            ..failure(
                StatusCode::TOO_MANY_REQUESTS,
                Kind::RateLimit,
                "rate_limit_exceeded",
                "Too many authentication failures from this address — verify your key and try again later",
            )
        }
    }

    pub fn unavailable() -> Self {
        Self {
            retry_after: Some(Duration::from_secs(1)),
            ..failure(
                StatusCode::SERVICE_UNAVAILABLE,
                Kind::Server,
                "service_unavailable",
                "Service temporarily unavailable — try again shortly",
            )
        }
    }

    /// Post-admission ambiguity: suppresses SDK auto-retry and directs caller to replay with the same key.
    pub fn outcome_unknown() -> Self {
        Self {
            no_retry: true,
            ..failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                Kind::Server,
                "request_outcome_unknown",
                "Could not confirm request outcome — query again with the same Idempotency-Key from the response header and the same request",
            )
        }
    }

    pub fn run_failed() -> Self {
        Self {
            no_retry: true,
            ..failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                Kind::Server,
                "run_failed",
                "Run failed",
            )
        }
    }

    pub fn run_cancelled() -> Self {
        Self {
            no_retry: true,
            ..failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                Kind::Server,
                "run_cancelled",
                "Run was cancelled",
            )
        }
    }

    pub fn body(&self) -> Value {
        json!({"error": {
            "message": self.message,
            "type": self.kind.name(),
            "param": self.param,
            "code": self.code,
        }})
    }

    pub fn sse_event(&self) -> String {
        format!("data: {}\n\n", self.body())
    }

    pub fn response(&self) -> Response {
        let mut response = crate::http::reply::json_response(self.status, &self.body());
        if let Some(wait) = self.retry_after {
            response = crate::http::reply::retry_after(response, wait);
        }
        let headers = response.headers_mut();
        if self.no_retry {
            headers.insert(
                HeaderName::from_static("x-should-retry"),
                HeaderValue::from_static("false"),
            );
        }
        if let Some(allow) = self.allow {
            headers.insert(header::ALLOW, HeaderValue::from_static(allow));
        }
        if self.challenge {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

#[cfg(test)]
mod tests;
