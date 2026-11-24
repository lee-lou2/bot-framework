use super::*;
use crate::testing::{Answer, Recorder, Server, serve_mcp};
use serde_json::json;
use std::sync::Arc;
use tokio::io::duplex;

const SHORT: Duration = Duration::from_millis(300);

fn schema() -> Value {
    json!({"type": "object", "properties": {"event": {"type": "string"}}})
}

/// 가짜 서버 한 대와 이어진 세션. 서버 태스크는 세션이 살아 있는 동안 계속 읽는다.
fn connect(server: Server) -> (Session, Arc<Recorder>) {
    let (to_server, from_client) = duplex(1 << 16);
    let (to_client, from_server) = duplex(1 << 16);
    let recorder = Arc::new(Recorder::default());
    tokio::spawn(serve_mcp(
        server,
        vec![],
        None,
        Box::new(from_client),
        Box::new(to_client),
        recorder.clone(),
    ));
    let session = Session::new(Pipe {
        read: Box::new(from_server),
        write: Box::new(to_server),
    });
    (session, recorder)
}

#[tokio::test]
async fn negotiates_a_supported_version_then_lists_and_calls() {
    let (mut s, rec) = connect(Server::new(&[("delete", schema())]));
    s.initialize(SHORT).await.unwrap();
    assert_eq!(
        s.tool_schema("delete", SHORT).await.unwrap(),
        Some(schema())
    );
    assert_eq!(s.tool_schema("missing", SHORT).await.unwrap(), None);
    let id = s
        .send_call("delete", r#"{"event":"x"}"#, r#"{"bot/call-id":"c1"}"#)
        .await
        .unwrap();
    let result = s.await_result(id, SHORT).await.unwrap();
    assert_eq!(result["content"][0]["text"], "ok");
    assert_eq!(rec.calls.lock().unwrap()[0]["_meta"]["bot/call-id"], "c1");
}

#[tokio::test]
async fn an_unsupported_protocol_version_is_refused() {
    for version in ["2099-01-01", "2024-01-01", "", "latest"] {
        let (mut s, _) = connect(Server::new(&[]).version(version));
        assert_eq!(
            s.initialize(SHORT).await,
            Err(McpError::Version),
            "{version:?}"
        );
    }
    for version in SUPPORTED {
        let (mut s, _) = connect(Server::new(&[]).version(version));
        assert_eq!(s.initialize(SHORT).await, Ok(()), "{version}");
    }
}

#[tokio::test]
async fn a_tool_listed_twice_is_ambiguous() {
    let (mut s, _) = connect(Server::new(&[("t", schema()), ("t", json!({}))]));
    s.initialize(SHORT).await.unwrap();
    assert_eq!(s.tool_schema("t", SHORT).await, Err(McpError::Protocol));
}

#[tokio::test]
async fn requests_from_the_server_are_refused_and_notifications_ignored_while_waiting() {
    let before = vec![
        r#"{"jsonrpc":"2.0","id":99,"method":"roots/list"}"#.to_owned(),
        r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#.to_owned(),
        r#"{"jsonrpc":"2.0","id":"s-1","method":"sampling/createMessage","params":{}}"#.to_owned(),
        r#"{"jsonrpc":"2.0","id":777,"result":{"stale":true}}"#.to_owned(),
    ];
    let server = Server::new(&[("t", schema())]).replying(move || {
        Answer::Before(
            before.clone(),
            Box::new(Answer::Result(
                json!({"content": [{"type": "text", "text": "ok"}]}),
            )),
        )
    });
    let (mut s, rec) = connect(server);
    s.initialize(SHORT).await.unwrap();
    let id = s.send_call("t", "{}", "{}").await.unwrap();
    assert_eq!(
        s.await_result(id, SHORT).await.unwrap()["content"][0]["text"],
        "ok"
    );
    // 세션이 열려 있는 동안 서버가 거절 응답을 읽는다
    for _ in 0..50 {
        if rec.client_replies.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let replies = rec.client_replies.lock().unwrap().clone();
    assert_eq!(replies.len(), 2);
    assert!(
        replies
            .iter()
            .all(|r| r["error"]["code"] == -32601 && r["jsonrpc"] == "2.0")
    );
    assert_eq!(
        (replies[0]["id"].clone(), replies[1]["id"].clone()),
        (json!(99), json!("s-1"))
    );
}

#[tokio::test]
async fn failures_are_classified() {
    let (mut s, _) = connect(Server::new(&[("t", schema())]).replying(|| Answer::Error(-32602)));
    s.initialize(SHORT).await.unwrap();
    let id = s.send_call("t", "{}", "{}").await.unwrap();
    assert_eq!(
        s.await_result(id, SHORT).await,
        Err(McpError::Server(-32602))
    );
    let (mut s, _) = connect(Server::new(&[("t", schema())]).replying(|| Answer::Hang));
    s.initialize(SHORT).await.unwrap();
    let id = s.send_call("t", "{}", "{}").await.unwrap();
    assert_eq!(
        s.await_result(id, Duration::from_millis(80)).await,
        Err(McpError::Timeout)
    );
    let (mut s, _) = connect(Server::new(&[("t", schema())]).replying(|| Answer::Close));
    s.initialize(SHORT).await.unwrap();
    let id = s.send_call("t", "{}", "{}").await.unwrap();
    assert_eq!(s.await_result(id, SHORT).await, Err(McpError::Closed));
}

#[tokio::test]
async fn garbage_and_oversized_frames_are_protocol_errors_not_crashes() {
    for line in [
        "not json".to_owned(),
        r#"{"jsonrpc":"1.0","id":1,"result":{}}"#.to_owned(),
        r#"{"jsonrpc":"2.0","id":2,"result":[]}"#.to_owned(),
        r#"{"jsonrpc":"2.0","id":2,"result":{},"id":2}"#.to_owned(),
        r#"[1,2]"#.to_owned(),
    ] {
        let sent = line.clone();
        let server = Server::new(&[("t", schema())])
            .replying(move || Answer::Before(vec![sent.clone()], Box::new(Answer::Hang)));
        let (mut s, _) = connect(server);
        s.initialize(SHORT).await.unwrap();
        let id = s.send_call("t", "{}", "{}").await.unwrap();
        assert!(
            matches!(s.await_result(id, SHORT).await, Err(McpError::Protocol)),
            "{line}"
        );
    }
    let huge = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"x\":\"{}\"}}}}",
        "a".repeat(MAX_FRAME)
    );
    let server = Server::new(&[("t", schema())])
        .replying(move || Answer::Before(vec![huge.clone()], Box::new(Answer::Hang)));
    let (mut s, _) = connect(server);
    s.initialize(SHORT).await.unwrap();
    let id = s.send_call("t", "{}", "{}").await.unwrap();
    assert_eq!(s.await_result(id, SHORT).await, Err(McpError::Io));
}

#[test]
fn only_text_results_in_the_expected_shape_are_accepted() {
    let ok = parse_result(&json!({"content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}], "isError": true})).unwrap();
    assert_eq!(
        ok,
        Output {
            is_error: true,
            texts: vec!["a".into(), "b".into()],
            structured: None,
            wake: None,
        }
    );
    assert_eq!(
        parse_result(&json!({"content": []})).unwrap().texts.len(),
        0
    );
    for bad in [
        json!({}),
        json!({"content": "text"}),
        json!({"content": [{"type": "image"}]}),
        json!({"content": [{"type": "text"}]}),
        json!({"content": [{"type": "text", "text": 5}]}),
        json!({"content": ["x"]}),
        json!({"content": [], "isError": 1}),
        json!([]),
    ] {
        assert_eq!(parse_result(&bad), Err("result_shape"), "{bad}");
    }
}

#[test]
fn the_model_facing_payload_has_a_fixed_canonical_shape() {
    let out = Output {
        is_error: false,
        texts: vec!["hi\n".into()],
        structured: Some(json!({"b": 1, "a": [1.5]})),
        wake: None,
    };
    assert_eq!(
        payload(&out).unwrap(),
        r#"{"content":[{"text":"hi\n","type":"text"}],"isError":false,"structuredContent":{"a":[1.5],"b":1}}"#
    );
    let unsafe_number = Output {
        is_error: false,
        texts: vec![],
        structured: Some(json!({"n": 1e300})),
        wake: None,
    };
    assert!(payload(&unsafe_number).is_none());
    assert_eq!(
        error_payload(),
        r#"{"content":[{"text":"tool_error","type":"text"}],"isError":true}"#
    );
}

#[test]
fn only_the_wake_request_survives_from_the_result_meta() {
    let wake = json!({"at": "2030-01-01T12:00:00Z", "instruction": "check", "quiet": true});
    let result = json!({
        "content": [{"type": "text", "text": "done"}],
        "_meta": {"bot/wake": wake.clone(), "progressToken": 7, "other": {"x": 1}}
    });
    let out = parse_result(&result).unwrap();
    assert_eq!(out.wake, Some(wake.clone()));
    // 모델에게 가는 모양에는 `bot/wake`만 실리고(브로커가 곧바로 떼어 낸다) 다른 `_meta`는 버린다
    assert_eq!(
        payload(&out).unwrap(),
        r#"{"_meta":{"bot/wake":{"at":"2030-01-01T12:00:00Z","instruction":"check","quiet":true}},"content":[{"text":"done","type":"text"}],"isError":false}"#
    );
    // `_meta`가 없거나 객체가 아니거나 키가 없으면 요청이 없다
    for result in [
        json!({"content": []}),
        json!({"content": [], "_meta": "text"}),
        json!({"content": [], "_meta": {"progressToken": 1}}),
        json!({"content": [], "_meta": []}),
    ] {
        let out = parse_result(&result).unwrap();
        assert_eq!(out.wake, None, "{result}");
        assert!(!payload(&out).unwrap().contains("_meta"));
    }
}
