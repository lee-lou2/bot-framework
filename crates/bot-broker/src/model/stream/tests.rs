use super::super::*;
use crate::testing::{End, FakeSecrets, FakeUpstream, ManualClock, MemLedger, run_context};
use bot_core::id::ClaimEpoch;

const KEY: &str = "provider-key-for-personal";

mod hold;
mod multi_protocol_tests;

struct Rig {
    proxy: ModelProxy,
    upstream: Arc<FakeUpstream>,
}

fn rig(limits: impl FnOnce(&mut ModelLimits)) -> Rig {
    let host = ModelHost {
        providers: [(
            "main".to_owned(),
            Provider {
                protocol: Protocol::Anthropic,
                endpoint: HostPort {
                    host: "api.main.example".into(),
                    port: 443,
                },
                path: "/v1/messages".into(),
                auth: bot_core::auth::AuthSource::Secret(SecretName::parse("model-key").unwrap()),
                header_mode: bot_core::auth::HeaderMode::ApiKey("x-api-key"),
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
        delta("café"),
        delta("€100"),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
    ];
    let all = events.concat().into_bytes();
    // Single-byte chunks split inside multi-byte UTF-8 characters.
    let chunks: Vec<&[u8]> = all.chunks(1).collect();
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    assert_eq!(collect(&mut stream).await, events);
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
    let (head, tail) = KEY.split_at(10);
    let events = [
        delta("prefix"),
        delta(head),
        "event: ping\ndata: {\"type\":\"ping\"}\n\n".to_owned(),
        delta(tail),
        delta("suffix"),
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
        "event completing the key must be withheld: {all}"
    );
    assert!(
        relayed.last().unwrap().contains("blocked"),
        "stream must terminate with a blocked error event"
    );
    assert!(!all.contains("suffix"), "stream must stop once blocked");
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
        // Keep concurrency generous because earlier streams remain open in this test.
        c.rate = RateConfig {
            per_minute: 6000,
            burst: 100,
            concurrent: 100,
        };
    });
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
        "no events may follow the error event"
    );
    r.upstream
        .respond_chunks(200, &[b"data: {\"cut\":"], End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert!(collect(&mut stream).await.concat().contains("truncated"));
    r.upstream
        .respond_chunks(200, &[b"data: {}\n\n"], End::Hang);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    assert!(collect(&mut stream).await.concat().contains("timeout"));
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
        "provider key header must appear once"
    );
    assert!(sent.iter().all(|(k, v)| !v.contains("session-token")
        && !v.contains("nope")
        && !v.contains("unlisted")
        && k != "host"
        && k != "authorization"));
    assert_eq!(
        sent.iter().filter(|(k, _)| k == "x-fixed").count(),
        1,
        "fixed provider header must not be overwritten"
    );
    let long = "a".repeat(600);
    for bad in ["bad\r\nx-evil: 1", "tab\tvalue", &long, "café"] {
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
