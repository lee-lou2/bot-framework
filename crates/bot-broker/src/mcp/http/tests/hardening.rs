//! Hardening tests: reject leaked credentials anywhere in responses, invalid cursors, mid-list changes, and mismatched 5xx errors.
use super::*;

const BEARER: &str = "bearer-value-123456";

fn small() -> Value {
    json!({"type": "object", "properties": {"q": {"type": "string"}}})
}

async fn list(fake: &Arc<FakeMcp>, bearer: &str) -> Result<Vec<Value>, McpError> {
    let (mut http, _wire) = client(fake, Some(bearer));
    http.connect(LIMIT).await?;
    http.tools(LIMIT).await
}

#[tokio::test]
async fn a_credential_anywhere_in_a_listing_is_refused_in_both_protocol_versions() {
    let escaped = "quote\"d-bearer-1234";
    for era in [Era::Modern, Era::Legacy] {
        let cases: Vec<(&str, &str, Vec<McpTool>)> = vec![
            (
                "description",
                BEARER,
                vec![McpTool::new("get", &format!("key {BEARER}"), small())],
            ),
            (
                "schema value",
                BEARER,
                vec![McpTool::new(
                    "get",
                    "d",
                    json!({"type": "object", "properties": {"q": {"type": "string", "enum": [BEARER]}}}),
                )],
            ),
            (
                "schema key",
                BEARER,
                vec![McpTool::new(
                    "get",
                    "d",
                    json!({"type": "object", "properties": {BEARER: {"type": "string"}}}),
                )],
            ),
            (
                "undeclared tool description",
                BEARER,
                vec![
                    McpTool::new("get", "d", small()),
                    McpTool::new("other", &format!("key {BEARER}"), small()),
                ],
            ),
            (
                "escaped string (differs from raw bytes)",
                escaped,
                vec![McpTool::new("get", &format!("key {escaped}"), small())],
            ),
            (
                "numeric literal",
                "123456789012",
                vec![McpTool::new(
                    "get",
                    "d",
                    json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 123456789012_u64}}}),
                )],
            ),
        ];
        for (label, bearer, tools) in cases {
            // Check both JSON responses and SSE streams.
            for sse in [false, true] {
                let mut fake = FakeMcp::new(era, tools.clone());
                fake.sse = sse;
                let fake = Arc::new(fake);
                let error = list(&fake, bearer).await.expect_err(label);
                assert_eq!(error, McpError::Exposed, "{era:?} sse={sse}: {label}");
            }
        }
    }
}

#[tokio::test]
async fn a_credential_in_a_cursor_is_never_sent_back_in_the_next_request() {
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.page = 1;
    fake.next_cursor = Some(json!(BEARER));
    let fake = Arc::new(fake);
    let error = list(&fake, BEARER).await.expect_err("credential in cursor");
    assert_eq!(error, McpError::Exposed);
    assert_eq!(fake.count("tools/list"), 1, "must not request next page");
    for request in fake.requests() {
        assert!(
            !request.body.contains(BEARER),
            "credential leaked into request body: {}",
            request.body
        );
    }
}

#[tokio::test]
async fn a_reply_carrying_the_credential_as_a_number_token_is_refused() {
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.echo_number = Some(123_456_789_012);
    let fake = Arc::new(fake);
    let (mut http, _wire) = client(&fake, Some("123456789012"));
    http.connect(LIMIT).await.unwrap();
    let error = http
        .call("get", r#"{"q":"x"}"#, "", Vec::new(), LIMIT)
        .await
        .expect_err("credential in numeric token");
    assert_eq!(error, McpError::Exposed);
}

#[tokio::test]
async fn an_invalid_next_cursor_is_an_error_not_the_last_page() {
    for bad in [
        json!(123),
        json!(true),
        json!({}),
        json!([]),
        json!(null),
        json!(""),
        json!("x".repeat(5000)),
    ] {
        let mut fake = FakeMcp::new(Era::Modern, tools());
        fake.next_cursor = Some(bad.clone());
        let fake = Arc::new(fake);
        let error = list(&fake, "unrelated-token-1234")
            .await
            .expect_err(&bad.to_string().chars().take(40).collect::<String>());
        assert_eq!(error, McpError::Protocol);
    }
}

#[tokio::test]
async fn a_listing_that_never_ends_stops_at_the_page_limit() {
    let many: Vec<McpTool> = (0..40)
        .map(|i| McpTool::new(&format!("tool{i}"), "d", small()))
        .collect();
    let mut fake = FakeMcp::new(Era::Modern, many);
    fake.page = 1;
    let fake = Arc::new(fake);
    let error = list(&fake, "unrelated-token-1234")
        .await
        .expect_err("page limit");
    assert_eq!(error, McpError::Protocol);
    assert!(fake.count("tools/list") <= 16);
}

#[tokio::test]
async fn a_listing_that_changes_while_it_is_collected_fails() {
    for era in [Era::Modern, Era::Legacy] {
        let mut fake = FakeMcp::new(era, tools());
        fake.sse = true;
        fake.list_changed = true;
        let fake = Arc::new(fake);
        let error = list(&fake, "unrelated-token-1234")
            .await
            .expect_err("list changed");
        assert_eq!(error, McpError::Changed, "{era:?}");
    }
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.sse = true;
    let fake = Arc::new(fake);
    assert_eq!(list(&fake, "unrelated-token-1234").await.unwrap().len(), 2);
}

#[tokio::test]
async fn only_a_2xx_reply_with_the_same_id_is_an_answer() {
    let other_id = r#"{"jsonrpc":"2.0","id":999,"error":{"code":-32602,"message":"x"}}"#;
    let same_id = |id: u64| {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32602,"message":"x"}}}}"#)
    };
    // A 5xx carrying a different request ID must not populate the error code.
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.fail = Some(("tools/call".into(), 500));
    fake.fail_body = Some(other_id.into());
    let fake = Arc::new(fake);
    let (mut http, _wire) = client(&fake, None);
    http.connect(LIMIT).await.unwrap();
    let error = http
        .call("get", "{}", "", Vec::new(), LIMIT)
        .await
        .expect_err("5xx");
    assert_eq!(
        error,
        McpError::Status {
            status: 500,
            code: None
        }
    );
    // A 5xx with a matching ID records the error code for negotiation but is still a status error.
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.fail = Some(("tools/call".into(), 500));
    // Request IDs on this connection: discover=1, call=2.
    fake.fail_body = Some(same_id(2));
    let fake = Arc::new(fake);
    let (mut http, _wire) = client(&fake, None);
    http.connect(LIMIT).await.unwrap();
    let error = http
        .call("get", "{}", "", Vec::new(), LIMIT)
        .await
        .expect_err("5xx");
    assert!(
        matches!(error, McpError::Status { status: 500, .. }),
        "{error:?}"
    );
}
