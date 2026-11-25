//! HTTP 응답 조립: 오류 코드와 도구 결과(`{status, body}`)의 검증·변환.
use axum::{
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use bot_core::command::JsonPayload;
use serde_json::{Value, json};
use std::time::Duration;

/// 도구 결과가 돌려줄 수 있는 최종 응답 코드. 인증(401)·속도(429)·서버 오류(5xx)는 게이트웨이만 만든다.
pub const ALLOWED_STATUS: [u16; 8] = [200, 201, 204, 400, 403, 404, 409, 422];
/// 도구 결과 본문의 최대 직렬화 크기
pub const RESULT_LIMIT: usize = 1 << 20;

pub fn json_response(status: StatusCode, value: &Value) -> Response {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = status;
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
    response
}

pub fn error(status: StatusCode, code: &str) -> Response {
    json_response(status, &json!({ "error": code }))
}

/// `Retry-After`(초, 올림, 최소 1)를 붙인다.
pub fn retry_after(mut response: Response, wait: Duration) -> Response {
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
    response
}

pub fn with_header(mut response: Response, name: header::HeaderName, value: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
    response
}

/// MCP `CallToolResult`(`{"content":[{"type":"text","text":"..."}],"isError":false}` 또는 `structuredContent`)에서
/// 도구 본문 JSON을 꺼낸다. `isError: true`면 거절한다(`None`). 이미 `{status, body}`이면 그대로 쓴다.
fn unwrap_mcp(value: Value) -> Option<Value> {
    let Value::Object(map) = &value else {
        return Some(value);
    };
    if map.len() == 2 && map.contains_key("status") && map.contains_key("body") {
        return Some(value);
    }
    if map.get("isError").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if let Some(sc) = map.get("structuredContent") {
        return Some(sc.clone());
    }
    if let Some([only]) = map
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        && only.get("type").and_then(Value::as_str) == Some("text")
        && let Some(text) = only.get("text").and_then(Value::as_str)
    {
        return serde_json::from_str(text).ok();
    }
    Some(value)
}

/// 도구 결과를 그대로 HTTP 응답으로 쓴다. 정확히 `{status, body}`(또는 이를 감싼 정상 MCP 결과)이고 상태가 허용 집합 안이며
/// 본문이 크기 한도 안일 때만 응답이 된다(204는 본문 `null`). 아니면 None.
pub fn tool_result(result: &JsonPayload) -> Option<Response> {
    let raw: Value = serde_json::from_str(&result.0).ok()?;
    let Value::Object(map) = unwrap_mcp(raw)? else {
        return None;
    };
    if map.len() != 2 {
        return None;
    }
    let status = u16::try_from(map.get("status")?.as_u64()?).ok()?;
    if !ALLOWED_STATUS.contains(&status) {
        return None;
    }
    let body = map.get("body")?;
    let status = StatusCode::from_u16(status).ok()?;
    if status == StatusCode::NO_CONTENT {
        if !body.is_null() {
            return None;
        }
        let mut response = Response::new(Body::empty());
        *response.status_mut() = status;
        return Some(with_header(response, header::CACHE_CONTROL, "no-store"));
    }
    (body.to_string().len() <= RESULT_LIMIT).then(|| json_response(status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(result: &str) -> Option<u16> {
        tool_result(&JsonPayload(result.into())).map(|r| r.status().as_u16())
    }

    #[test]
    fn passes_only_the_fixed_shape_with_an_allowed_status() {
        assert_eq!(
            status_of(r#"{"status":201,"body":{"id":"e-1"}}"#),
            Some(201)
        );
        assert_eq!(
            status_of(r#"{"status":404,"body":{"error":"gone"}}"#),
            Some(404)
        );
        assert_eq!(status_of(r#"{"status":204,"body":null}"#), Some(204));
        for bad in [
            r#"{"status":204,"body":{}}"#,
            r#"{"status":500,"body":{}}"#,
            r#"{"status":401,"body":{}}"#,
            r#"{"status":202,"body":{}}"#,
            r#"{"status":"200","body":{}}"#,
            r#"{"status":200}"#,
            r#"{"body":{}}"#,
            r#"{"status":200,"body":{},"headers":{}}"#,
            r#"[200,{}]"#,
            r#""ok""#,
            "not json",
        ] {
            assert_eq!(status_of(bad), None, "{bad}");
        }
        let huge = format!(r#"{{"status":200,"body":"{}"}}"#, "x".repeat(RESULT_LIMIT));
        assert_eq!(status_of(&huge), None);
        // MCP CallToolResult로 감싼 {status, body}도 정상 결과만 풀어 쓴다.
        assert_eq!(
            status_of(
                r#"{"content":[{"type":"text","text":"{\"status\":404,\"body\":{\"ok\":false}}"}],"isError":false}"#
            ),
            Some(404)
        );
        assert_eq!(
            status_of(r#"{"structuredContent":{"status":200,"body":{"ok":true}},"isError":false}"#),
            Some(200)
        );
        assert_eq!(
            status_of(
                r#"{"content":[{"type":"text","text":"{\"status\":200,\"body\":{\"ok\":true}}"}],"isError":true}"#
            ),
            None
        );
        assert_eq!(
            status_of(
                r#"{"content":[{"type":"text","text":"{\"status\":500,\"body\":{}}"}],"isError":false}"#
            ),
            None
        );
    }

    #[test]
    fn retry_after_rounds_up_and_is_at_least_one_second() {
        let wait = |d| {
            retry_after(error(StatusCode::TOO_MANY_REQUESTS, "rate_limited"), d)
                .headers()
                .get(header::RETRY_AFTER)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(wait(Duration::from_millis(10)), "1");
        assert_eq!(wait(Duration::from_millis(1500)), "2");
        assert_eq!(wait(Duration::from_secs(20)), "20");
        assert_eq!(wait(Duration::ZERO), "1");
    }
}
