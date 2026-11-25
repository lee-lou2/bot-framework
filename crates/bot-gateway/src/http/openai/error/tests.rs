use super::*;
use axum::body::to_bytes;

async fn read(failure: &Failure) -> (u16, Value, Response) {
    let response = failure.response();
    let status = response.status().as_u16();
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, 1 << 20).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    (status, value, Response::from_parts(parts, Body::empty()))
}

fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response.headers().get(name).and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn every_error_has_the_four_openai_fields_and_the_documented_status() {
    let table: Vec<(Failure, u16, &str, &str)> = vec![
        (
            Failure::invalid("x"),
            400,
            "invalid_request_error",
            "invalid_request",
        ),
        (
            Failure::unsupported("tools"),
            400,
            "invalid_request_error",
            "unsupported_parameter",
        ),
        (
            Failure::idempotency_conflict(),
            400,
            "invalid_request_error",
            "idempotency_key_conflict",
        ),
        (
            Failure::idempotency_required(),
            400,
            "invalid_request_error",
            "idempotency_key_required",
        ),
        (
            Failure::invalid_api_key(),
            401,
            "authentication_error",
            "invalid_api_key",
        ),
        (
            Failure::access_denied(),
            403,
            "permission_error",
            "access_denied",
        ),
        (
            Failure::model_not_found(),
            404,
            "invalid_request_error",
            "model_not_found",
        ),
        (
            Failure::not_found(),
            404,
            "invalid_request_error",
            "not_found",
        ),
        (
            Failure::method_not_allowed("POST"),
            405,
            "invalid_request_error",
            "method_not_allowed",
        ),
        (
            Failure::too_large(),
            413,
            "invalid_request_error",
            "request_too_large",
        ),
        (
            Failure::unsupported_media(),
            415,
            "invalid_request_error",
            "unsupported_media_type",
        ),
        (
            Failure::read_timeout(),
            408,
            "invalid_request_error",
            "request_timeout",
        ),
        (
            Failure::rate_limited(Duration::from_millis(1500)),
            429,
            "rate_limit_error",
            "rate_limit_exceeded",
        ),
        (
            Failure::unavailable(),
            503,
            "server_error",
            "service_unavailable",
        ),
        (
            Failure::outcome_unknown(),
            422,
            "server_error",
            "request_outcome_unknown",
        ),
        (Failure::run_failed(), 422, "server_error", "run_failed"),
        (
            Failure::run_cancelled(),
            422,
            "server_error",
            "run_cancelled",
        ),
    ];
    for (failure, status, kind, code) in table {
        let (got, body, response) = read(&failure).await;
        assert_eq!(
            (
                got,
                body["error"]["type"].as_str(),
                body["error"]["code"].as_str()
            ),
            (status, Some(kind), Some(code))
        );
        let error = body["error"].as_object().unwrap();
        assert_eq!(error.len(), 4, "message·type·param·code만: {body}");
        assert!(error["message"].as_str().is_some_and(|m| !m.is_empty()));
        assert!(error.contains_key("param"));
        assert_eq!(header(&response, "content-type"), Some("application/json"));
        assert_eq!(header(&response, "cache-control"), Some("no-store"));
    }
}

#[tokio::test]
async fn retry_hints_are_set_only_where_resending_cannot_start_a_second_run() {
    // 접수 전 한도·장애: SDK가 기다렸다 다시 보내도 된다(Retry-After는 올림, 최소 1초)
    let (_, _, limited) = read(&Failure::rate_limited(Duration::from_millis(1500))).await;
    assert_eq!(header(&limited, "retry-after"), Some("2"));
    let (_, _, tiny) = read(&Failure::rate_limited(Duration::from_millis(10))).await;
    assert_eq!(header(&tiny, "retry-after"), Some("1"));
    let (_, _, unavailable) = read(&Failure::unavailable()).await;
    assert_eq!(header(&unavailable, "retry-after"), Some("1"));
    assert_eq!(header(&unavailable, "x-should-retry"), None);
    // 접수 뒤의 불명·실패·취소: 다시 보내지 않게 한다
    for failure in [
        Failure::outcome_unknown(),
        Failure::run_failed(),
        Failure::run_cancelled(),
    ] {
        let (status, _, response) = read(&failure).await;
        assert_eq!(status, 422);
        assert_eq!(header(&response, "x-should-retry"), Some("false"));
        assert_eq!(header(&response, "retry-after"), None);
    }
    // 그 밖의 오류는 재시도 표지가 없다
    for failure in [
        Failure::invalid("x"),
        Failure::access_denied(),
        Failure::model_not_found(),
    ] {
        let (_, _, response) = read(&failure).await;
        assert_eq!(header(&response, "x-should-retry"), None);
    }
}

#[tokio::test]
async fn challenge_allow_and_param_headers_follow_the_status() {
    let (_, _, unauthorized) = read(&Failure::invalid_api_key()).await;
    assert_eq!(header(&unauthorized, "www-authenticate"), Some("Bearer"));
    let (_, _, method) = read(&Failure::method_not_allowed("GET, HEAD")).await;
    assert_eq!(header(&method, "allow"), Some("GET, HEAD"));
    let (_, body, _) = read(&Failure::unsupported("messages[0].content[1].type")).await;
    assert_eq!(body["error"]["param"], "messages[0].content[1].type");
    let (_, body, _) = read(&Failure::invalid("x")).await;
    assert!(body["error"]["param"].is_null());
    assert_eq!(Failure::model_not_found().body()["error"]["param"], "model");
}

#[test]
fn the_sse_error_event_is_one_json_line_and_a_blank_line() {
    let event = Failure::outcome_unknown().sse_event();
    assert!(event.starts_with("data: {\"error\":{") && event.ends_with("}\n\n"));
    assert_eq!(event.matches('\n').count(), 2, "{event:?}");
    let data = event.strip_prefix("data: ").unwrap().trim_end();
    let value: Value = serde_json::from_str(data).unwrap();
    assert_eq!(value["error"]["code"], "request_outcome_unknown");
}

#[test]
fn messages_never_echo_inputs_or_internals() {
    for failure in [
        Failure::invalid("요청 본문이 JSON이 아니에요"),
        Failure::unsupported("tools"),
        Failure::idempotency_conflict(),
        Failure::idempotency_required(),
        Failure::invalid_api_key(),
        Failure::access_denied(),
        Failure::model_not_found(),
        Failure::outcome_unknown(),
    ] {
        let message = failure.message;
        for forbidden in ["sk-", "nonce", "postgres", "panicked", "Bearer"] {
            assert!(!message.contains(forbidden), "{message}");
        }
    }
}
