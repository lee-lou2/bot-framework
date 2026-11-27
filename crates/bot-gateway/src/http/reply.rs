//! HTTP response builders and `{status, body}` tool-result validation and conversion.
use axum::{
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use bot_core::command::JsonPayload;
use serde_json::{Value, json};
use std::time::Duration;

/// Status codes tools may return directly; auth (401), rate-limit (429), and 5xx belong to the gateway.
pub const ALLOWED_STATUS: [u16; 8] = [200, 201, 204, 400, 403, 404, 409, 422];
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

/// Attaches `Retry-After` rounded up to whole seconds, minimum 1.
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

/// Unwraps non-error MCP `CallToolResult` envelopes into the underlying tool JSON; rejects `isError: true`.
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

/// Converts a strict `{status, body}` tool result (204 requires `body: null`) within `RESULT_LIMIT` into an HTTP response.
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
