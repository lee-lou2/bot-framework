//! 검토에서 나온 재현: 서버 응답은 믿을 수 없다. 자격이 응답 어디에 섞여 있든(원문·글 값·키·숫자 표기·커서) 쓰지 않고, 잘못된 커서·도중에 바뀐 목록은 오류이며,
//! 쓰기 응답은 2xx이고 같은 요청 번호일 때만 결과로 인정한다.
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
                "설명",
                BEARER,
                vec![McpTool::new("get", &format!("키 {BEARER}"), small())],
            ),
            (
                "스키마 값",
                BEARER,
                vec![McpTool::new(
                    "get",
                    "d",
                    json!({"type": "object", "properties": {"q": {"type": "string", "enum": [BEARER]}}}),
                )],
            ),
            (
                "스키마 키",
                BEARER,
                vec![McpTool::new(
                    "get",
                    "d",
                    json!({"type": "object", "properties": {BEARER: {"type": "string"}}}),
                )],
            ),
            (
                "선언하지 않은 도구의 설명",
                BEARER,
                vec![
                    McpTool::new("get", "d", small()),
                    McpTool::new("other", &format!("키 {BEARER}"), small()),
                ],
            ),
            (
                "이스케이프가 필요한 글(원문에는 다른 모양으로 있다)",
                escaped,
                vec![McpTool::new("get", &format!("키 {escaped}"), small())],
            ),
            (
                "숫자 표기",
                "123456789012",
                vec![McpTool::new(
                    "get",
                    "d",
                    json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 123456789012_u64}}}),
                )],
            ),
        ];
        for (label, bearer, tools) in cases {
            // JSON 응답과 SSE 스트림 모두에서(스트림은 이벤트마다 원문과 해석한 값을 본다)
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
    let error = list(&fake, BEARER).await.expect_err("커서의 자격");
    assert_eq!(error, McpError::Exposed);
    assert_eq!(fake.count("tools/list"), 1, "다음 쪽을 요청하지 않는다");
    for request in fake.requests() {
        assert!(
            !request.body.contains(BEARER),
            "본문에 자격이 실렸어요: {}",
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
        .expect_err("숫자 토큰의 자격");
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
        .expect_err("페이지 상한");
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
            .expect_err("목록 변경");
        assert_eq!(error, McpError::Changed, "{era:?}");
    }
    // 알림이 없으면(같은 서버) 모은다
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
    // 5xx에 다른 요청 번호의 거절이 실려 있다: 그 코드는 이 요청의 결과가 아니다
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
    // 같은 번호의 오류 본문이면 코드는 읽되(판본 협상에 쓴다) 결과로 인정하지는 않는다
    let mut fake = FakeMcp::new(Era::Modern, tools());
    fake.fail = Some(("tools/call".into(), 500));
    // 연결에 쓰인 요청 번호(discover 1, 이어서 call 2)
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
