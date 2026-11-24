use super::super::*;
use crate::testing::{End, FakeSecrets, FakeUpstream, ManualClock, MemLedger, run_context};
use bot_core::id::ClaimEpoch;

const KEY: &str = "provider-key-for-personal";

mod hold;

struct Rig {
    proxy: ModelProxy,
    upstream: Arc<FakeUpstream>,
}

fn rig(limits: impl FnOnce(&mut ModelLimits)) -> Rig {
    let host = ModelHost {
        providers: [(
            "main".to_owned(),
            Provider {
                endpoint: HostPort {
                    host: "api.main.example".into(),
                    port: 443,
                },
                path: "/v1/messages".into(),
                secret: SecretName::parse("model-key").unwrap(),
                header: "x-api-key".into(),
                scheme: None,
                headers: vec![("x-fixed".into(), "1".into())],
                forward_headers: vec!["anthropic-version".into(), "x-fixed".into()],
                body_fields: vec![],
                models: [("assistant".to_owned(), "model-large".to_owned())].into(),
            },
        )]
        .into(),
        allow: [(ProfileId("personal".into()), ["main".to_owned()].into())].into(),
    };
    let clock = Arc::new(ManualClock::new(1_000));
    let ledger = Arc::new(MemLedger::new(clock.clone()));
    ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &["run"],
    ));
    let upstream = Arc::new(FakeUpstream::new());
    let mut cfg = ModelLimits::default();
    limits(&mut cfg);
    let proxy = ModelProxy::new(
        host,
        ledger,
        Arc::new(FakeSecrets::new(&[("personal", "model-key", KEY)])),
        upstream.clone(),
        clock,
        cfg,
    );
    Rig { proxy, upstream }
}

fn request(body: &str, headers: &[(&str, &str)]) -> ModelRequest {
    ModelRequest {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            generation: Generation(1),
            claim: Some(ClaimEpoch(1)),
        },
        alias: "assistant".into(),
        body: JsonPayload(body.into()),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

const STREAM_BODY: &str = r#"{"messages":[],"stream":true}"#;

async fn collect(stream: &mut ModelStream) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(event) = stream.next().await {
        out.push(String::from_utf8(event).unwrap());
    }
    out
}

fn delta(text: &str) -> String {
    format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
    )
}

#[tokio::test]
async fn events_are_relayed_whole_even_when_chunks_cut_them_anywhere() {
    let r = rig(|_| {});
    let events = [
        delta("안녕"),
        delta("하세요"),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
    ];
    let all = events.concat().into_bytes();
    // 바이트 하나씩 잘라 보내도(한글 중간 포함) 이벤트가 온전히 나온다
    let chunks: Vec<&[u8]> = all.chunks(1).collect();
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    assert_eq!(collect(&mut stream).await, events);
    // CRLF 구분도 같은 이벤트로 본다
    r.upstream.respond_chunks(
        200,
        &[b"data: {\"a\":1}\r\n\r\ndata: {\"b\":2}\r\n\r\nevent: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n"],
        End::Eof,
    );
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert_eq!(
        collect(&mut stream).await,
        [
            "data: {\"a\":1}\r\n\r\n",
            "data: {\"b\":2}\r\n\r\n",
            "event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n"
        ]
    );
}

#[tokio::test]
async fn the_key_goes_upstream_only_and_the_request_is_a_streaming_one() {
    let r = rig(|_| {});
    r.upstream.respond_chunks(200, &[b"data: {}\n\n"], End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(
            r#"{"model":"attacker","stream":true,"messages":[]}"#,
            &[],
        ))
        .await
        .unwrap();
    collect(&mut stream).await;
    let (endpoint, path, headers, body) = &r.upstream.requests()[0];
    assert_eq!(
        (endpoint.as_str(), path.as_str()),
        ("api.main.example:443", "/v1/messages")
    );
    assert!(headers.contains(&("x-api-key".into(), KEY.into())));
    assert!(headers.contains(&("accept".into(), "text/event-stream".into())));
    assert_eq!(
        body,
        r#"{"messages":[],"model":"model-large","stream":true}"#
    );
}

#[tokio::test]
async fn a_streaming_request_must_say_so_and_a_plain_one_must_not() {
    let r = rig(|_| {});
    for body in [
        r#"{"messages":[]}"#,
        r#"{"stream":false}"#,
        r#"{"stream":"yes"}"#,
        "[1]",
    ] {
        assert!(
            matches!(
                r.proxy.forward_stream(request(body, &[])).await,
                Err(Error::Invalid)
            ),
            "{body}"
        );
    }
    assert!(matches!(
        r.proxy.forward(request(STREAM_BODY, &[])).await,
        Err(Error::Invalid)
    ));
    assert!(r.upstream.requests().is_empty());
}

#[tokio::test]
async fn a_key_cut_by_chunk_boundaries_never_leaves_the_proxy() {
    // 키 바이트가 조각 경계에 걸쳐 흘러도(이벤트 안에서도, 이벤트 사이에서도) 어느 조각도 내보내기 전에 잡는다
    let data = format!("data: {{\"text\":\"{KEY}\"}}\n\n").into_bytes();
    for cut in 1..data.len() {
        let r = rig(|_| {});
        let (a, b) = data.split_at(cut);
        r.upstream
            .respond_chunks(200, &[b"data: {}\n\n", a, b], End::Eof);
        let mut stream = r
            .proxy
            .forward_stream(request(STREAM_BODY, &[]))
            .await
            .unwrap();
        let all = collect(&mut stream).await.concat();
        assert!(
            !all.contains(KEY) && !all.contains("provider-key"),
            "cut {cut}: {all}"
        );
        assert!(all.starts_with("data: {}\n\n"));
        assert!(
            all.ends_with("\"message\":\"blocked\"}}\n\n"),
            "cut {cut}: {all}"
        );
    }
}

#[tokio::test]
async fn an_escaped_key_is_caught_after_decoding() {
    let escaped = KEY.replacen('p', "\\u0070", 1);
    let r = rig(|_| {});
    r.upstream.respond_chunks(
        200,
        &[format!("data: {{\"text\":\"{escaped}\"}}\n\n").as_bytes()],
        End::Eof,
    );
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    let all = collect(&mut stream).await.concat();
    assert!(
        !all.contains("rovider-key") && all.contains("blocked"),
        "{all}"
    );
    // 키 이름에 숨겨도 막는다
    r.upstream.respond_chunks(
        200,
        &[format!("data: {{\"{escaped}\":1}}\n\n").as_bytes()],
        End::Eof,
    );
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert!(collect(&mut stream).await.concat().contains("blocked"));
}

#[tokio::test]
async fn a_key_split_across_delta_events_is_caught_before_it_completes() {
    let r = rig(|_| {});
    // 같은 JSON 경로(delta.text)의 값 여러 개에 나눠 담아 흘린다. 다른 경로의 값이 사이에 끼어도 소용없다.
    let (head, tail) = KEY.split_at(10);
    let events = [
        delta("앞부분"),
        delta(head),
        "event: ping\ndata: {\"type\":\"ping\"}\n\n".to_owned(),
        delta(tail),
        delta("뒷부분"),
    ];
    let chunks: Vec<&[u8]> = events.iter().map(|e| e.as_bytes()).collect();
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    let relayed = collect(&mut stream).await;
    let all = relayed.concat();
    assert!(
        !all.contains(KEY) && !all.contains(tail),
        "키가 완성되는 이벤트는 나가지 않는다: {all}"
    );
    assert!(
        relayed.last().unwrap().contains("blocked"),
        "오류 이벤트로 끝난다"
    );
    assert!(!all.contains("뒷부분"), "막은 뒤 스트림은 끝난다");
}

#[tokio::test]
async fn non_json_data_and_other_event_lines_are_scanned_too() {
    let r = rig(|_| {});
    let (head, tail) = KEY.split_at(8);
    r.upstream.respond_chunks(
        200,
        &[format!("data: {head}\n\ndata: {tail}\n\n").as_bytes()],
        End::Eof,
    );
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert!(!collect(&mut stream).await.concat().contains(tail));
    r.upstream.respond_chunks(
        200,
        &[format!("event: {KEY}\ndata: {{}}\n\n").as_bytes()],
        End::Eof,
    );
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert!(!collect(&mut stream).await.concat().contains(KEY));
    // 문자열이 아닌 UTF-8 깨진 이벤트는 내보내지 않는다
    r.upstream
        .respond_chunks(200, &[b"data: \xff\xfe\n\n"], End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert!(collect(&mut stream).await.concat().contains("blocked"));
}

#[tokio::test]
async fn failures_end_the_stream_with_one_fixed_error_event() {
    let r = rig(|c| {
        c.stream_idle = std::time::Duration::from_millis(80);
        c.max_event = 64;
        c.max_stream = 400;
        // 앞서 연 스트림을 이 시험이 쥐고 있으므로 동시성 자리를 넉넉히 둔다.
        c.rate = RateConfig {
            per_minute: 6000,
            burst: 100,
            concurrent: 100,
        };
    });
    // 상류가 중간에 끊겼다
    r.upstream
        .respond_chunks(200, &[b"data: {}\n\n"], End::Fail);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    let got = collect(&mut stream).await;
    assert_eq!(got.len(), 2);
    assert!(got[1].contains("\"message\":\"unavailable\""));
    assert!(
        stream.next().await.is_none(),
        "오류 뒤에는 더 나오지 않는다"
    );
    // 이벤트가 마무리되지 않은 채 끝났다
    r.upstream
        .respond_chunks(200, &[b"data: {\"cut\":"], End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert!(collect(&mut stream).await.concat().contains("truncated"));
    // 조각 사이가 너무 오래 비었다
    r.upstream
        .respond_chunks(200, &[b"data: {}\n\n"], End::Hang);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert!(collect(&mut stream).await.concat().contains("timeout"));
    // 이벤트 하나가 너무 크다 / 전체가 너무 크다
    r.upstream.respond_chunks(200, &[&[b'a'; 200]], End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert!(collect(&mut stream).await.concat().contains("too_large"));
    let small = b"data: {\"a\":1}\n\n";
    let many: Vec<&[u8]> = (0..40).map(|_| &small[..]).collect();
    r.upstream.respond_chunks(200, &many, End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    let all = collect(&mut stream).await;
    assert!(all.last().unwrap().contains("too_large") && all.len() < 40);
}

#[tokio::test]
async fn the_whole_stream_has_a_time_limit() {
    let r = rig(|c| {
        c.stream_total = std::time::Duration::from_millis(120);
        c.stream_idle = std::time::Duration::from_secs(30);
    });
    r.upstream
        .respond_chunks(200, &[b"data: {}\n\n"], End::Hang);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    let all = tokio::time::timeout(std::time::Duration::from_secs(5), collect(&mut stream))
        .await
        .unwrap();
    assert!(all.last().unwrap().contains("timeout"));
}

#[tokio::test]
async fn an_upstream_error_status_becomes_a_fixed_body_without_the_upstream_text() {
    let r = rig(|_| {});
    r.upstream.respond_chunks(
        429,
        &[b"{\"error\":\"account acct-123 is over quota\"}"],
        End::Eof,
    );
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert_eq!(stream.status(), 429);
    assert_eq!(collect(&mut stream).await, [r#"{"error":{"status":429}}"#]);
    // 상류에 닿지 못하면 스트림이 아니라 오류다
    r.upstream.fail();
    assert!(matches!(
        r.proxy.forward_stream(request(STREAM_BODY, &[])).await,
        Err(Error::Unavailable)
    ));
}

#[tokio::test]
async fn a_stream_holds_its_concurrency_slot_until_it_is_dropped() {
    let r = rig(|c| {
        c.rate = RateConfig {
            per_minute: 6000,
            burst: 100,
            concurrent: 1,
        }
    });
    r.upstream
        .respond_chunks(200, &[b"data: {}\n\n"], End::Hang);
    let stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert!(matches!(
        r.proxy.forward_stream(request(STREAM_BODY, &[])).await,
        Err(Error::Unavailable)
    ));
    drop(stream);
    r.upstream.respond_chunks(200, &[b"data: {}\n\n"], End::Eof);
    assert!(
        r.proxy
            .forward_stream(request(STREAM_BODY, &[]))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn only_operator_allowed_client_headers_pass_and_values_are_vetted() {
    let r = rig(|_| {});
    let headers = [
        ("Anthropic-Version", "2023-06-01"),
        ("x-api-key", "session-token-must-not-pass"),
        ("authorization", "Bearer nope"),
        ("anthropic-beta", "unlisted-feature"),
        ("x-fixed", "override-attempt"),
        ("host", "evil.example"),
    ];
    r.upstream.respond_chunks(200, &[b"data: {}\n\n"], End::Eof);
    collect(
        &mut r
            .proxy
            .forward_stream(request(STREAM_BODY, &headers))
            .await
            .unwrap(),
    )
    .await;
    let sent = &r.upstream.requests()[0].2;
    assert!(sent.contains(&("anthropic-version".into(), "2023-06-01".into())));
    assert_eq!(
        sent.iter().filter(|(k, _)| k == "x-api-key").count(),
        1,
        "키 머리는 우리 것 하나뿐이다"
    );
    assert!(sent.iter().all(|(k, v)| !v.contains("session-token")
        && !v.contains("nope")
        && !v.contains("unlisted")
        && k != "host"
        && k != "authorization"));
    assert_eq!(
        sent.iter().filter(|(k, _)| k == "x-fixed").count(),
        1,
        "고정 머리는 덮어쓰지 못한다"
    );
    // 값에 제어 문자·너무 긴 값이 있으면 버린다
    let long = "a".repeat(600);
    for bad in ["bad\r\nx-evil: 1", "tab\tvalue", &long, "한글"] {
        r.upstream.respond_chunks(200, &[b"data: {}\n\n"], End::Eof);
        collect(
            &mut r
                .proxy
                .forward_stream(request(STREAM_BODY, &[("anthropic-version", bad)]))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            r.upstream
                .requests()
                .last()
                .unwrap()
                .2
                .iter()
                .all(|(k, _)| k != "anthropic-version"),
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn the_non_streaming_path_forwards_the_same_allowed_headers() {
    let r = rig(|_| {});
    r.proxy
        .forward(request(
            r#"{"messages":[]}"#,
            &[("anthropic-version", "2023-06-01")],
        ))
        .await
        .unwrap();
    assert!(
        r.upstream.requests()[0]
            .2
            .contains(&("anthropic-version".into(), "2023-06-01".into()))
    );
}
