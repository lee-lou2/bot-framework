use super::*;

fn rig_for(protocol: Protocol, path: &str, header: &str, scheme: Option<&str>) -> Rig {
    let host = ModelHost {
        providers: [(
            "main".to_owned(),
            Provider {
                protocol,
                endpoint: HostPort {
                    host: "api.main.example".into(),
                    port: 443,
                },
                path: path.into(),
                secret: SecretName::parse("model-key").unwrap(),
                header: header.into(),
                scheme: scheme.map(str::to_owned),
                headers: vec![],
                forward_headers: vec![],
                body_fields: vec![],
                models: [("assistant".to_owned(), "configured-model".to_owned())].into(),
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
    let proxy = ModelProxy::new(
        host,
        ledger,
        Arc::new(FakeSecrets::new(&[("personal", "model-key", KEY)])),
        upstream.clone(),
        clock,
        ModelLimits::default(),
    );
    Rig { proxy, upstream }
}

#[tokio::test]
async fn responses_stream_terminates_on_completed_and_blocks_split_secret() {
    let r = rig_for(
        Protocol::OpenAiResponses,
        "/v1/responses",
        "authorization",
        Some("Bearer"),
    );
    let events = [
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
    ];
    let chunks: Vec<&[u8]> = events.iter().map(|s| s.as_bytes()).collect();
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(
            r#"{"model":"attacker","stream":true,"store":false,"input":[]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(collect(&mut stream).await, events);
    let (_, path, headers, body) = &r.upstream.requests()[0];
    assert_eq!(path, "/v1/responses");
    assert!(headers.contains(&("authorization".into(), format!("Bearer {KEY}"))));
    assert!(body.contains("\"model\":\"configured-model\""), "{body}");
    assert!(body.contains("\"store\":false"), "{body}");

    // Omitting `store` still pins `"store":false` on the upstream request (H2).
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(r#"{"stream":true,"input":[]}"#, &[]))
        .await
        .unwrap();
    collect(&mut stream).await;
    assert!(r.upstream.requests()[1].3.contains("\"store\":false"));

    let (head, tail) = KEY.split_at(10);
    let leak = [
        format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{head}\"}}\n\n"
        ),
        format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{tail}\"}}\n\n"
        ),
    ];
    r.upstream
        .respond_chunks(200, &[leak[0].as_bytes(), leak[1].as_bytes()], End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(r#"{"stream":true,"input":[]}"#, &[]))
        .await
        .unwrap();
    let all = collect(&mut stream).await.concat();
    assert!(!all.contains(KEY) && !all.contains(tail), "{all}");
    assert!(
        all.contains("\"code\":\"permission_denied\"") && all.contains("\"message\":\"blocked\""),
        "{all}"
    );
}

#[tokio::test]
async fn gemini_stream_pins_model_path_flushes_after_finish_reason_and_blocks_leaks() {
    let r = rig_for(Protocol::Gemini, "/v1beta", "x-goog-api-key", None);
    let chunks = [
        b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}]}}]}\n\n".as_slice(),
        b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}]},\"finishReason\":\"STOP\"}]}\n\n".as_slice(),
    ];
    r.upstream.respond_chunks(200, &chunks, End::Eof);
    let mut stream = r
        .proxy
        .forward_stream(request(r#"{"contents":[]}"#, &[]))
        .await
        .unwrap();
    let got = collect(&mut stream).await;
    drop(stream);
    assert_eq!(got.len(), 2);
    let (_, path, headers, body) = &r.upstream.requests()[0];
    assert_eq!(
        path,
        "/v1beta/models/configured-model:streamGenerateContent?alt=sse"
    );
    assert!(headers.contains(&("x-goog-api-key".into(), KEY.into())));
    assert!(
        !body.contains("\"model\"") && !body.contains("\"stream\""),
        "{body}"
    );

    // Non-streaming Gemini uses `:generateContent`.
    r.proxy
        .forward(request(r#"{"contents":[]}"#, &[]))
        .await
        .unwrap();
    assert_eq!(
        r.upstream.requests()[1].1,
        "/v1beta/models/configured-model:generateContent"
    );

    // EOF before `finishReason` terminates with Gemini-formatted `truncated`.
    r.upstream.respond_chunks(
        200,
        &[b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"cut\"}]}}]}\n\n"],
        End::Eof,
    );
    let mut cut = r
        .proxy
        .forward_stream(request(r#"{"contents":[]}"#, &[]))
        .await
        .unwrap();
    let cut_all = collect(&mut cut).await.concat();
    drop(cut);
    assert!(
        cut_all.contains("\"status\":\"UNAVAILABLE\"")
            && cut_all.contains("\"message\":\"truncated\""),
        "{cut_all}"
    );

    // Secret split across chunks is blocked with Gemini 403 error.
    let (head, tail) = KEY.split_at(11);
    let leak = [
        format!(
            "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{head}\"}}]}}}}]}}\n\n"
        ),
        format!(
            "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{tail}\"}}]}},\"finishReason\":\"STOP\"}}]}}\n\n"
        ),
    ];
    r.upstream
        .respond_chunks(200, &[leak[0].as_bytes(), leak[1].as_bytes()], End::Eof);
    let mut blocked = r
        .proxy
        .forward_stream(request(r#"{"contents":[]}"#, &[]))
        .await
        .unwrap();
    let blocked_all = collect(&mut blocked).await.concat();
    drop(blocked);
    assert!(
        !blocked_all.contains(KEY) && !blocked_all.contains(tail),
        "{blocked_all}"
    );
    assert!(
        blocked_all.contains("\"code\":403")
            && blocked_all.contains("\"status\":\"PERMISSION_DENIED\"")
            && blocked_all.contains("\"message\":\"blocked\""),
        "{blocked_all}"
    );

    // Secret split across finishReason and a trailing chunk stays held until blocked (H3).
    let leak_after_finish = [
        format!(
            "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{head}\"}}]}},\"finishReason\":\"STOP\"}}]}}\n\n"
        ),
        format!(
            "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{tail}\"}}]}}}}]}}\n\n"
        ),
    ];
    r.upstream.respond_chunks(
        200,
        &[
            leak_after_finish[0].as_bytes(),
            leak_after_finish[1].as_bytes(),
        ],
        End::Eof,
    );
    let mut held_block = r
        .proxy
        .forward_stream(request(r#"{"contents":[]}"#, &[]))
        .await
        .unwrap();
    let held_all = collect(&mut held_block).await.concat();
    drop(held_block);
    assert!(
        !held_all.contains(head) && !held_all.contains(tail) && !held_all.contains(KEY),
        "held prefix must not flush on finishReason before EOF: {held_all}"
    );
    assert!(held_all.contains("\"message\":\"blocked\""), "{held_all}");

    // Non-2xx upstream Gemini error emits ProtoJSON `code`/`message`/`status` (M2).
    r.upstream
        .respond_chunks(429, &[b"{\"error\":\"raw quota info\"}"], End::Eof);
    let mut err_stream = r
        .proxy
        .forward_stream(request(r#"{"contents":[]}"#, &[]))
        .await
        .unwrap();
    assert_eq!(err_stream.status(), 429);
    assert_eq!(
        collect(&mut err_stream).await,
        [r#"{"error":{"code":429,"message":"upstream_error","status":"RESOURCE_EXHAUSTED"}}"#]
    );
}
