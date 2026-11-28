use super::*;
use crate::front::{cli::PlannedCalls, observe::CheckpointSink};
use bot_core::{
    cli::{
        CliGroup, CliResponse, CliUse, GROUP_PREFIX, RESPONSE_PREFIX, USE_PREFIX, codex_response_id,
    },
    command::{
        Accept, AcceptExtras, Beat, Completion, Failure, Finish, Retry, RunOutcome, Step, Turn,
    },
    intake::{RequestKey, RequestState, RunProgress},
    port::{Error, Reply as PortReply, RunStore},
    state::RunContext,
};

/// `RunStore` double mirroring `checkpoint_batch` directly into `MemLedger` without auto-expanding `cli-resp-*`.
struct DirectRuns {
    ledger: Arc<MemLedger>,
}

impl RunStore for DirectRuns {
    fn accept_with(&self, _: Accept, _: AcceptExtras) -> PortReply<'_, RunId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn status(&self, _: RequestKey) -> PortReply<'_, Option<RequestState>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn progress(&self, _: ProfileId, _: RunId) -> PortReply<'_, Option<RunProgress>> {
        Box::pin(async { Ok(None) })
    }
    fn claim(&self) -> PortReply<'_, Option<RunContext>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn release_for_shutdown(&self, _: Attempt) -> PortReply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn renew(&self, _: Attempt) -> PortReply<'_, Beat> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn checkpoint_batch(&self, attempt: Attempt, steps: Vec<Step>) -> PortReply<'_, ()> {
        for step in steps {
            self.ledger
                .add_step(&attempt.run.0, &step.key, &step.body.0);
        }
        Box::pin(async { Ok(()) })
    }
    fn finish(&self, _: Attempt, _: Finish) -> PortReply<'_, Completion> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn finish_with(&self, a: Attempt, done: Finish, _: RunOutcome) -> PortReply<'_, Completion> {
        self.finish(a, done)
    }
    fn fail(&self, _: Attempt, _: Failure) -> PortReply<'_, Retry> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn history(&self, _: Attempt, _: u32) -> PortReply<'_, Vec<Turn>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

struct ProtoRig {
    front: ModelFront,
    sessions: Arc<SessionTable>,
    upstream: Arc<FakeUpstream>,
    runner: Arc<FakeRunner>,
    ledger: Arc<MemLedger>,
    token: String,
    attempt: Attempt,
}

async fn proto_rig(protocol: Protocol, planning_runs: bool) -> ProtoRig {
    let clock = Arc::new(ManualClock::new(1_000));
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(world());
    let ledger = Arc::new(MemLedger::new(clock.clone()));
    ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &["run", "echo.use", "echo.drop"],
    ));
    ledger.set_active_revision("personal", "r1");
    let runner = Arc::new(FakeRunner::new());
    let broker = Arc::new(BrokerService::new(
        Parts {
            catalog: catalog.clone(),
            memory: Arc::new(MemMemory::new(ledger.clone())),
            ledger: ledger.clone(),
            effects: Arc::new(FakeEffects::new()),
            audit: Arc::new(MemAudit::new()),
            runner: runner.clone(),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: clock.clone(),
            entropy: Arc::new(CountingEntropy::default()),
        },
        Config::default(),
    ));
    let (path, header_mode) = match protocol {
        Protocol::Anthropic => (
            "/v1/messages",
            bot_core::auth::HeaderMode::ApiKey("x-api-key"),
        ),
        Protocol::OpenAiResponses => ("/v1/responses", bot_core::auth::HeaderMode::Bearer),
        Protocol::Gemini => (
            "/v1beta",
            bot_core::auth::HeaderMode::ApiKey("x-goog-api-key"),
        ),
    };
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
                auth: bot_core::auth::AuthSource::Secret(SecretName::parse("model-key").unwrap()),
                header_mode,
                headers: vec![],
                forward_headers: vec![],
                body_fields: vec![],
                models: [("assistant".to_owned(), "pinned-model".to_owned())].into(),
            },
        )]
        .into(),
        allow: [(ProfileId("personal".into()), ["main".to_owned()].into())].into(),
    };
    let upstream = Arc::new(FakeUpstream::new());
    let model = Arc::new(ModelProxy::new(
        host,
        ledger.clone(),
        Arc::new(FakeSecrets::new(&[("personal", "model-key", KEY)])),
        upstream.clone(),
        clock.clone(),
        ModelLimits::default(),
    ));
    let sessions = SessionTable::new(Arc::new(CountingEntropy::default()));
    let attempt = Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        generation: Generation(1),
        claim: Some(ClaimEpoch(1)),
    };
    let token = sessions
        .issue(Session {
            attempt: attempt.clone(),
            agent: AgentId("main".into()),
            alias: "assistant".into(),
            protocol,
        })
        .unwrap();
    let runs: Arc<dyn RunStore> = if planning_runs {
        Arc::new(PlanningRuns::new(ledger.clone()))
    } else {
        Arc::new(DirectRuns {
            ledger: ledger.clone(),
        })
    };
    let front = ModelFront::start(
        "127.0.0.1:0".parse().unwrap(),
        FrontParts {
            sessions: sessions.clone(),
            model,
            broker: broker.clone(),
            catalog,
            calls: Arc::new(
                PlannedCalls::new(ledger.clone(), Duration::from_secs(2))
                    .with_preflight(broker.clone()),
            ),
            turns: Arc::new(CheckpointSink::new(runs)),
            ledger: ledger.clone(),
            delegation: Arc::new(FakeDelegation::new()),
            park: Arc::new(RecordingPark::new()),
        },
        FrontConfig::default(),
    )
    .await
    .unwrap();
    ProtoRig {
        front,
        sessions,
        upstream,
        runner,
        ledger,
        token,
        attempt,
    }
}

async fn mcp_call(addr: SocketAddr, token: &str, body: &str) -> Reply {
    let bearer = format!("Bearer {token}");
    raw(addr, &post("/mcp", Some(&bearer), "authorization", body)).await
}

#[tokio::test]
async fn codex_responses_turn_commits_plan_and_authorizes_mcp_call_by_call_id() {
    let r = proto_rig(Protocol::OpenAiResponses, true).await;
    let events = [
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_x\"}}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_codex_1\",\"namespace\":\"mcp__relay\",\"name\":\"echo_say\",\"arguments\":\"\"}}\n\n",
        "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"msg\\\":\\\"hi\\\"}\"}\n\n",
        "event: response.function_call_arguments.done\ndata: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{\\\"msg\\\":\\\"hi\\\"}\"}\n\n",
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_codex_1\",\"namespace\":\"mcp__relay\",\"name\":\"echo_say\",\"arguments\":\"{\\\"msg\\\":\\\"hi\\\"}\"}}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_x\",\"output\":[{\"type\":\"function_call\",\"call_id\":\"call_codex_1\",\"namespace\":\"mcp__relay\",\"name\":\"echo_say\",\"arguments\":\"{\\\"msg\\\":\\\"hi\\\"}\"}]}}\n\n",
    ];
    let chunks: Vec<&[u8]> = events.iter().map(|s| s.as_bytes()).collect();
    r.upstream.respond_chunks(200, &chunks, End::Eof);

    let bearer = format!("Bearer {}", r.token);
    let wrong = raw(
        r.front.addr(),
        &post("/v1/messages", Some(&bearer), "authorization", r#"{}"#),
    )
    .await;
    assert_eq!(wrong.status, 404);
    assert!(r.upstream.requests().is_empty());

    let reply = raw(
        r.front.addr(),
        &post(
            "/v1/responses",
            Some(&bearer),
            "authorization",
            r#"{"model":"ignored","stream":true,"store":false,"input":[]}"#,
        ),
    )
    .await;
    assert_eq!(reply.status, 200);
    let expected_resp_id = codex_response_id("call_codex_1");
    let stored = r
        .ledger
        .step(
            r.attempt.clone(),
            format!("{RESPONSE_PREFIX}{expected_resp_id}"),
        )
        .await
        .unwrap()
        .expect("cli-resp checkpoint committed");
    let resp: CliResponse = serde_json::from_str(&stored.0).unwrap();
    assert_eq!(resp.uses[0].name, "mcp__relay__echo_say");
    assert_eq!(resp.uses[0].id, "call_codex_1");

    let call = mcp_call(
        r.front.addr(),
        &r.token,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo_say","arguments":{"msg":"hi"},"_meta":{"callId":"call_codex_1"}}}"#,
    )
    .await;
    assert_eq!(call.status, 200);
    assert!(
        call.body.contains("ok") && !call.body.contains("\"isError\":true"),
        "{}",
        call.body
    );
    let jobs = r.runner.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].call, CallId::cli(1, 0));
}

#[tokio::test]
async fn agy_gemini_turn_commits_directly_matches_fifo_progress_tokens_and_rejects_forgery() {
    let r = proto_rig(Protocol::Gemini, false).await;
    // Parallel duplicate calls in one Gemini chunk verify FIFO slot allocation and idempotent retry.
    let sse = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[",
        "{\"functionCall\":{\"name\":\"call_mcp_tool\",\"args\":{\"ServerName\":\"relay\",\"ToolName\":\"echo_say\",\"Arguments\":{\"x\":1}}}},",
        "{\"functionCall\":{\"name\":\"call_mcp_tool\",\"args\":{\"ServerName\":\"relay\",\"ToolName\":\"echo_say\",\"Arguments\":{\"x\":1}}}}",
        "]},\"finishReason\":\"STOP\"}]}\n\n"
    );
    r.upstream.respond_chunks(200, &[sse.as_bytes()], End::Eof);

    let wrong = raw(
        r.front.addr(),
        &post("/v1/responses", Some(&r.token), "x-goog-api-key", r#"{}"#),
    )
    .await;
    assert_eq!(wrong.status, 404);
    assert!(
        wrong.body.contains("\"status\":\"NOT_FOUND\""),
        "{}",
        wrong.body
    );

    let reply = raw(
        r.front.addr(),
        &post(
            "/v1beta/models/gemini-3.1-pro-preview:streamGenerateContent?alt=sse",
            Some(&r.token),
            "x-goog-api-key",
            r#"{"contents":[]}"#,
        ),
    )
    .await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        r.upstream.requests()[0].1,
        "/v1beta/models/pinned-model:streamGenerateContent?alt=sse"
    );

    let group_raw = r
        .ledger
        .step(r.attempt.clone(), format!("{GROUP_PREFIX}0"))
        .await
        .unwrap()
        .expect("cli-group-0 committed directly");
    let group: CliGroup = serde_json::from_str(&group_raw.0).unwrap();
    assert_eq!(group.seq, 0);
    assert_eq!(group.uses, ["agy-0-0", "agy-0-1"]);
    let u0: CliUse = serde_json::from_str(
        &r.ledger
            .step(r.attempt.clone(), format!("{USE_PREFIX}agy-0-0"))
            .await
            .unwrap()
            .unwrap()
            .0,
    )
    .unwrap();
    assert_eq!(
        (u0.turn, u0.pos, u0.name.as_str()),
        (0, 0, "mcp__relay__echo_say")
    );

    // First parallel call -> agy-0-0 -> CallId::cli(0, 0).
    let c1 = mcp_call(
        r.front.addr(),
        &r.token,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo_say","arguments":{"x":1},"_meta":{"progressToken":"traj-1:2"}}}"#,
    )
    .await;
    assert!(!c1.body.contains("\"isError\":true"), "{}", c1.body);
    assert_eq!(r.runner.jobs()[0].call, CallId::cli(0, 0));
    // Idempotent retry with the same progressToken replays slot 0 without running a second job.
    let c1_retry = mcp_call(
        r.front.addr(),
        &r.token,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo_say","arguments":{"x":1},"_meta":{"progressToken":"traj-1:2"}}}"#,
    )
    .await;
    assert!(
        !c1_retry.body.contains("\"isError\":true"),
        "{}",
        c1_retry.body
    );
    assert_eq!(r.runner.jobs().len(), 1);
    // Same progressToken with tampered args is refused.
    let tamper = mcp_call(
        r.front.addr(),
        &r.token,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo_say","arguments":{"x":2},"_meta":{"progressToken":"traj-1:2"}}}"#,
    )
    .await;
    assert!(
        tamper.body.contains("conflict") && tamper.body.contains("\"isError\":true"),
        "{}",
        tamper.body
    );

    // Second parallel call -> agy-0-1 -> CallId::cli(0, 1).
    let c2 = mcp_call(
        r.front.addr(),
        &r.token,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"echo_say","arguments":{"x":1},"_meta":{"progressToken":"traj-1:3"}}}"#,
    )
    .await;
    assert!(!c2.body.contains("\"isError\":true"), "{}", c2.body);
    assert_eq!(r.runner.jobs().len(), 2);
    assert_eq!(r.runner.jobs()[1].call, CallId::cli(0, 1));

    // Extra unobserved call -> refused with conflict, zero execution.
    let forged = mcp_call(
        r.front.addr(),
        &r.token,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"echo_say","arguments":{"x":1},"_meta":{"progressToken":"traj-1:4"}}}"#,
    )
    .await;
    assert!(
        forged.body.contains("conflict") && forged.body.contains("\"isError\":true"),
        "{}",
        forged.body
    );
    assert_eq!(r.runner.jobs().len(), 2);

    // Missing progressToken -> invalid.
    let missing_tok = mcp_call(
        r.front.addr(),
        &r.token,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"echo_say","arguments":{"x":1}}}"#,
    )
    .await;
    assert!(
        missing_tok.body.contains("invalid") && missing_tok.body.contains("\"isError\":true"),
        "{}",
        missing_tok.body
    );

    // Resumed attempt (new claim epoch) probes ledger, skips existing cli-group-0, and commits cli-group-1.
    r.ledger.bump_claim("r1");
    let next_attempt = Attempt {
        claim: Some(r.ledger.claim_epoch("r1")),
        ..r.attempt.clone()
    };
    let token2 = r
        .sessions
        .issue(Session {
            attempt: next_attempt.clone(),
            agent: AgentId("main".into()),
            alias: "assistant".into(),
            protocol: Protocol::Gemini,
        })
        .unwrap();
    let sse2 = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"call_mcp_tool\",\"args\":{\"ServerName\":\"relay\",\"ToolName\":\"echo_say\",\"Arguments\":{\"y\":2}}}}]},\"finishReason\":\"STOP\"}]}\n\n";
    r.upstream.respond_chunks(200, &[sse2.as_bytes()], End::Eof);
    let reply2 = raw(
        r.front.addr(),
        &post(
            "/v1beta/models/gemini-3.1-pro-preview:streamGenerateContent?alt=sse",
            Some(&token2),
            "x-goog-api-key",
            r#"{"contents":[]}"#,
        ),
    )
    .await;
    assert_eq!(reply2.status, 200);
    let group1: CliGroup = serde_json::from_str(
        &r.ledger
            .step(next_attempt, format!("{GROUP_PREFIX}1"))
            .await
            .unwrap()
            .expect("cli-group-1 committed on resumed attempt")
            .0,
    )
    .unwrap();
    assert_eq!(group1.seq, 1);
    assert_eq!(group1.uses, ["agy-1-0"]);
}

#[tokio::test]
async fn agy_observer_rejects_foreign_server_post_finish_call_and_secondary_candidate_calls() {
    let r = proto_rig(Protocol::Gemini, false).await;
    let foreign = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"call_mcp_tool\",\"args\":{\"ServerName\":\"evil\",\"ToolName\":\"echo_say\",\"Arguments\":{}}}}]},\"finishReason\":\"STOP\"}]}\n\n";
    let post_finish = [
        b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"done\"}]},\"finishReason\":\"STOP\"}]}\n\n".as_slice(),
        b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"call_mcp_tool\",\"args\":{\"ServerName\":\"relay\",\"ToolName\":\"echo_say\",\"Arguments\":{}}}}]}}]}\n\n".as_slice(),
    ];
    let second_candidate = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"},{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"call_mcp_tool\",\"args\":{\"ServerName\":\"relay\",\"ToolName\":\"echo_say\",\"Arguments\":{}}}}]}}]}\n\n";

    for chunks in [
        vec![foreign.as_bytes()],
        post_finish.to_vec(),
        vec![second_candidate.as_bytes()],
    ] {
        r.upstream.respond_chunks(200, &chunks, End::Eof);
        let reply = raw(
            r.front.addr(),
            &post(
                "/v1beta/models/gemini-3.1-pro-preview:streamGenerateContent?alt=sse",
                Some(&r.token),
                "x-goog-api-key",
                r#"{"contents":[]}"#,
            ),
        )
        .await;
        assert!(
            reply.body.contains("\"message\":\"unavailable\""),
            "{}",
            reply.body
        );
    }
    assert!(
        r.ledger
            .step(r.attempt.clone(), format!("{GROUP_PREFIX}0"))
            .await
            .unwrap()
            .is_none(),
        "rejected Gemini turns must never commit a plan"
    );
}

#[tokio::test]
async fn agy_new_turn_expires_prior_unbound_slots_and_hallucinated_tool_name_consumes_its_slot() {
    let r = proto_rig(Protocol::Gemini, false).await;
    // Turn 0 emits echo_say (left unexecuted) + hallucinated invalid ToolId name.
    let turn0 = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[",
        "{\"functionCall\":{\"name\":\"call_mcp_tool\",\"args\":{\"ServerName\":\"relay\",\"ToolName\":\"echo_say\",\"Arguments\":{\"x\":1}}}},",
        "{\"functionCall\":{\"name\":\"call_mcp_tool\",\"args\":{\"ServerName\":\"relay\",\"ToolName\":\"bad_tool_name\",\"Arguments\":{}}}}",
        "]},\"finishReason\":\"STOP\"}]}\n\n"
    );
    r.upstream
        .respond_chunks(200, &[turn0.as_bytes()], End::Eof);
    assert_eq!(
        raw(
            r.front.addr(),
            &post(
                "/v1beta/models/gemini-3.1-pro-preview:streamGenerateContent?alt=sse",
                Some(&r.token),
                "x-goog-api-key",
                r#"{"contents":[]}"#,
            ),
        )
        .await
        .status,
        200
    );
    // Calling the hallucinated name consumes slot agy-0-1 and returns invalid (M1).
    let bad = mcp_call(
        r.front.addr(),
        &r.token,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"bad_tool_name","arguments":{},"_meta":{"progressToken":"traj-1:2"}}}"#,
    )
    .await;
    assert!(
        bad.body.contains("invalid") && bad.body.contains("\"isError\":true"),
        "{}",
        bad.body
    );

    // Turn 1 emits echo_say {"x":1}; prior unbound agy-0-0 must NOT be stolen (H4).
    let turn1 = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"call_mcp_tool\",\"args\":{\"ServerName\":\"relay\",\"ToolName\":\"echo_say\",\"Arguments\":{\"x\":1}}}}]},\"finishReason\":\"STOP\"}]}\n\n";
    r.upstream
        .respond_chunks(200, &[turn1.as_bytes()], End::Eof);
    assert_eq!(
        raw(
            r.front.addr(),
            &post(
                "/v1beta/models/gemini-3.1-pro-preview:streamGenerateContent?alt=sse",
                Some(&r.token),
                "x-goog-api-key",
                r#"{"contents":[]}"#,
            ),
        )
        .await
        .status,
        200
    );
    let c = mcp_call(
        r.front.addr(),
        &r.token,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo_say","arguments":{"x":1},"_meta":{"progressToken":"traj-1:5"}}}"#,
    )
    .await;
    assert!(!c.body.contains("\"isError\":true"), "{}", c.body);
    let jobs = r.runner.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].call,
        CallId::cli(1, 0),
        "must bind to turn 1, not stale turn 0"
    );
}
