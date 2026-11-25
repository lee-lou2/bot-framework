//! OpenAI 오류 형식 `{"error":{"message","type","param","code"}}`와 상태 코드 표. 문구는 모두 고정이다 — 요청 원문·키·내부 오류·승인 식별자를 되풀이하지 않는다.
//!
//! SDK의 자동 재시도(연결 오류, 408·409·429·5xx)는 **아직 아무것도 접수하지 않았을 때만** 일어나게 한다: 접수 뒤의 실패·불명은 422에
//! `x-should-retry: false`를 붙여 같은 요청이 새 실행으로 되풀이되지 않게 하고, 429는 접수 전 속도·실행 한도에만 쓴다.
use axum::{
    body::Body,
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
    /// 잘못된 필드 경로(예: `messages[0].content`). 값은 싣지 않는다.
    pub param: Option<String>,
    pub retry_after: Option<Duration>,
    /// 응답 헤더 `x-should-retry: false` — SDK가 이 응답을 다시 보내지 않는다
    pub no_retry: bool,
    /// 405의 `Allow`
    pub allow: Option<&'static str>,
    /// 401의 `WWW-Authenticate: Bearer`
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

    /// 400 — 잘못된 JSON·중복 키·요청 형태·식별자·지원 필드의 값
    pub fn invalid(message: &'static str) -> Self {
        failure(
            StatusCode::BAD_REQUEST,
            Kind::InvalidRequest,
            "invalid_request",
            message,
        )
    }

    /// 400 — 지원하지 않는 필드·역할·파트·옵션. 경로가 비어 있으면 `param`은 null이다.
    pub fn unsupported(param: impl Into<String>) -> Self {
        let param = param.into();
        let failure = failure(
            StatusCode::BAD_REQUEST,
            Kind::InvalidRequest,
            "unsupported_parameter",
            "이 서버가 지원하지 않는 필드나 값이에요",
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
            "같은 Idempotency-Key로 다른 요청이 이미 있어요 — 다른 요청에는 새 키를 쓰세요",
        )
    }

    /// 키 없이 온 SDK 재시도(`x-stainless-retry-count > 0`): 새 실행으로 되풀이하지 않는다
    pub fn idempotency_required() -> Self {
        failure(
            StatusCode::BAD_REQUEST,
            Kind::InvalidRequest,
            "idempotency_key_required",
            "다시 보낸 요청에는 처음 요청의 Idempotency-Key가 필요해요 — 새 실행을 만들지 않았어요",
        )
    }

    pub fn invalid_api_key() -> Self {
        Self {
            challenge: true,
            ..failure(
                StatusCode::UNAUTHORIZED,
                Kind::Authentication,
                "invalid_api_key",
                "API 키가 없거나 올바르지 않아요",
            )
        }
    }

    pub fn access_denied() -> Self {
        failure(
            StatusCode::FORBIDDEN,
            Kind::Permission,
            "access_denied",
            "이 키로는 요청을 처리하거나 결과를 볼 수 없어요",
        )
    }

    /// 없는 모델·접근할 수 없는 모델·접수했던 바인딩이 다른 모델로 재지정됨: 모두 같은 문서
    pub fn model_not_found() -> Self {
        failure(
            StatusCode::NOT_FOUND,
            Kind::InvalidRequest,
            "model_not_found",
            "모델이 없거나 이 키로 쓸 수 없어요",
        )
        .with_param("model")
    }

    pub fn not_found() -> Self {
        failure(
            StatusCode::NOT_FOUND,
            Kind::InvalidRequest,
            "not_found",
            "지원하지 않는 경로예요",
        )
    }

    pub fn method_not_allowed(allow: &'static str) -> Self {
        Self {
            allow: Some(allow),
            ..failure(
                StatusCode::METHOD_NOT_ALLOWED,
                Kind::InvalidRequest,
                "method_not_allowed",
                "이 경로가 지원하지 않는 메서드예요",
            )
        }
    }

    pub fn too_large() -> Self {
        failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            Kind::InvalidRequest,
            "request_too_large",
            "요청 본문이 한도를 넘었어요",
        )
    }

    pub fn unsupported_media() -> Self {
        failure(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Kind::InvalidRequest,
            "unsupported_media_type",
            "Content-Type은 application/json이어야 해요",
        )
    }

    /// 접수 전 요청 읽기 기한 — 아직 아무것도 접수하지 않았으니 SDK가 다시 보내도 된다
    pub fn read_timeout() -> Self {
        failure(
            StatusCode::REQUEST_TIMEOUT,
            Kind::InvalidRequest,
            "request_timeout",
            "요청을 제때 다 받지 못했어요",
        )
    }

    /// 접수 전 속도·실행 한도 — 아무것도 접수하지 않았다
    pub fn rate_limited(wait: Duration) -> Self {
        Self {
            retry_after: Some(wait),
            ..failure(
                StatusCode::TOO_MANY_REQUESTS,
                Kind::RateLimit,
                "rate_limit_exceeded",
                "요청이 한도를 넘었어요 — 잠시 뒤 다시 시도하세요",
            )
        }
    }

    /// 429 — 이 출처에서 키 인증이 여러 번 실패해 한동안 거절한다(비밀 보관소를 읽지 않았다). 키가 맞는지는 알리지 않는다.
    pub fn auth_blocked(wait: Duration) -> Self {
        Self {
            retry_after: Some(wait),
            ..failure(
                StatusCode::TOO_MANY_REQUESTS,
                Kind::RateLimit,
                "rate_limit_exceeded",
                "이 주소에서 인증에 여러 번 실패해 잠시 막았어요 — 키를 확인하고 잠시 뒤 다시 시도하세요",
            )
        }
    }

    /// 확실한 접수 전의 저장소 등 장애
    pub fn unavailable() -> Self {
        Self {
            retry_after: Some(Duration::from_secs(1)),
            ..failure(
                StatusCode::SERVICE_UNAVAILABLE,
                Kind::Server,
                "service_unavailable",
                "지금은 요청을 받을 수 없어요 — 잠시 뒤 다시 시도하세요",
            )
        }
    }

    /// 접수 커밋·종결을 확인할 수 없다: 같은 키와 같은 요청으로 다시 조회하게 하고 SDK는 새로 보내지 않는다
    pub fn outcome_unknown() -> Self {
        Self {
            no_retry: true,
            ..failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                Kind::Server,
                "request_outcome_unknown",
                "요청이 접수됐는지 확인하지 못했어요 — 응답 헤더의 Idempotency-Key와 같은 요청으로 다시 조회하세요",
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
                "작업이 실패했어요",
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
                "작업이 취소됐어요",
            )
        }
    }

    /// `{"error":{...}}` 본문
    pub fn body(&self) -> Value {
        json!({"error": {
            "message": self.message,
            "type": self.kind.name(),
            "param": self.param,
            "code": self.code,
        }})
    }

    /// 헤더가 이미 나간 SSE 스트림에서 쓰는 오류 사건(한 줄 JSON + 빈 줄)
    pub fn sse_event(&self) -> String {
        format!("data: {}\n\n", self.body())
    }

    pub fn response(&self) -> Response {
        let mut response = Response::new(Body::from(self.body().to_string()));
        *response.status_mut() = self.status;
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        if let Some(wait) = self.retry_after {
            let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
            headers.insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
        }
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
