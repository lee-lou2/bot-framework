//! End-to-end canary exercising a real Claude Code CLI against the broker HTTP front (SSE + MCP) with fake upstream and zero egress.
//!
//! Run: `BOT_CLAUDE_BIN=<path> BOT_CLAUDE_CANARY=required cargo test -p bot-broker --features testing --test claude_canary`
//! Set `BOT_CLAUDE_CANARY=required` to fail when `claude` is absent; otherwise the test skips.

#![cfg(all(unix, feature = "testing"))]

use bot_broker::{
    egress::EgressPolicy,
    front::{
        DirectIds, FrontConfig, FrontParts, ModelFront, Session, SessionTable,
        cli::PlannedCalls,
        observe::{CheckpointSink, NoSink},
        park::NoPark,
    },
    model::{ModelHost, ModelLimits, ModelProxy, Provider},
    ports::Ran,
    proxy::{Proxy, ProxyConfig},
    resolve::HostPort,
    service::{BrokerService, Config, Parts},
    testing::*,
};
use bot_core::{command::JsonPayload, id::*, state::Risk};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

const KEY: &str = "provider-key-for-personal";

fn required() -> bool {
    std::env::var("BOT_CLAUDE_CANARY").is_ok_and(|v| v == "required")
}

fn claude_bin() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BOT_CLAUDE_BIN") {
        return Some(path.into());
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| PathBuf::from(d).join("claude"))
        .find(|p| p.is_file())
}

fn event(name: &str, data: &str) -> Vec<u8> {
    format!("event: {name}\ndata: {data}\n\n").into_bytes()
}

fn tool_use_turn() -> Vec<Vec<u8>> {
    vec![
        event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_t1","type":"message","role":"assistant","model":"model-large","content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        ),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_canary1","name":"mcp__relay__echo_say","input":{}}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"text\":\"hel"}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"lo\"}"}}"#,
        ),
        event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":4}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

fn final_turn(text: &str) -> Vec<Vec<u8>> {
    vec![
        event("message_start", r#"{"type":"message_start","message":{"id":"msg_t2","type":"message","role":"assistant","model":"model-large","content":[],"stop_reason":null,"usage":{"input_tokens":9,"output_tokens":1}}}"#),
        event("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
        event("content_block_delta", &format!(r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}"#).into_bytes().iter().map(|b| *b as char).collect::<String>()),
        event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        event("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}"#),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

struct Harness {
    front: ModelFront,
    upstream: Arc<FakeUpstream>,
    runner: Arc<FakeRunner>,
    ledger: Arc<MemLedger>,
    token: String,
}

async fn harness() -> Harness {
    harness_with(false).await
}

async fn harness_with(planned: bool) -> Harness {
    let clock = Arc::new(ManualClock::new(1_000));
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &["run", "echo.use"])
            .member("local:owner", &["owner"])
            .policy_scopes(&["run", "echo.use"])
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
            .build(),
    );
    let ledger = Arc::new(MemLedger::new(clock.clone()));
    ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &["run", "echo.use"],
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
                headers: vec![],
                forward_headers: vec!["anthropic-version".into(), "anthropic-beta".into()],
                body_fields: vec![],
                models: [("assistant".to_owned(), "model-large".to_owned())].into(),
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
        clock,
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
            attempt,
            agent: AgentId("main".into()),
            alias: "assistant".into(),
        })
        .unwrap();
    let front = ModelFront::start(
        "127.0.0.1:0".parse().unwrap(),
        FrontParts {
            sessions,
            model,
            broker,
            catalog,
            calls: if planned {
                Arc::new(PlannedCalls::new(ledger.clone(), Duration::from_secs(10)))
            } else {
                Arc::new(DirectIds)
            },
            turns: if planned {
                Arc::new(CheckpointSink::new(Arc::new(PlanningRuns::new(
                    ledger.clone(),
                ))))
            } else {
                Arc::new(NoSink)
            },
            ledger: ledger.clone(),
            delegation: Arc::new(FakeDelegation::new()),
            park: Arc::new(NoPark),
        },
        FrontConfig::default(),
    )
    .await
    .unwrap();
    Harness {
        front,
        upstream,
        runner,
        ledger,
        token,
    }
}

async fn run_claude(
    bin: &PathBuf,
    h: &Harness,
    deny_proxy: &Proxy,
    extra: &[&str],
    limit: Duration,
) -> (Option<i32>, String, String) {
    let home = tempfile::tempdir().unwrap();
    let base = format!("http://{}", h.front.addr());
    let sink = format!("http://{}", deny_proxy.addr());
    let mcp = serde_json::json!({"mcpServers": {"relay": {"type": "http", "url": format!("{base}/mcp"), "headers": {"Authorization": format!("Bearer {}", h.token)}}}});
    let mut command = Command::new(bin);
    command
        .args([
            "--bare",
            "-p",
            "say hello with the tool",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
        ])
        .args([
            "--tools",
            "",
            "--model",
            "model-large",
            "--strict-mcp-config",
            "--allowedTools",
            "mcp__relay__echo_say",
        ])
        .arg("--mcp-config")
        .arg(mcp.to_string())
        .args(extra)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("CLAUDE_CONFIG_DIR", home.path().join("cfg"))
        .env("TMPDIR", home.path())
        .env("ANTHROPIC_BASE_URL", &base)
        .env("ANTHROPIC_API_KEY", &h.token)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("DISABLE_BUG_COMMAND", "1")
        .env("HTTPS_PROXY", &sink)
        .env("HTTP_PROXY", &sink)
        .env("https_proxy", &sink)
        .env("http_proxy", &sink)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("failed to spawn claude");
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let buffers = (
        Arc::new(std::sync::Mutex::new(String::new())),
        Arc::new(std::sync::Mutex::new(String::new())),
    );
    let (out_buf, err_buf) = (buffers.0.clone(), buffers.1.clone());
    let out_task = tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = out.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            out_buf
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    });
    let err_task = tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = err.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            err_buf
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    });
    // Kill and return collected output on timeout because the CLI backs off across non-streaming retries.
    let status = match timeout(limit, child.wait()).await {
        Ok(status) => status.ok().and_then(|s| s.code()),
        Err(_) => {
            let _ = child.kill().await;
            None
        }
    };
    let _ = timeout(Duration::from_secs(2), async {
        let _ = out_task.await;
        let _ = err_task.await;
    })
    .await;
    let (stdout, stderr) = (
        buffers.0.lock().unwrap().clone(),
        buffers.1.lock().unwrap().clone(),
    );
    (status, stdout, stderr)
}

async fn deny_all_proxy() -> Proxy {
    let policy = EgressPolicy::new(&[], None);
    let cfg = ProxyConfig::new(policy, "x".into(), "y".into());
    Proxy::start(
        cfg,
        Arc::new(FakeResolver::new(&[])),
        Arc::new(FakeConnector::new()),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn the_real_cli_makes_a_tool_round_trip_through_the_front_with_zero_outside_traffic() {
    let Some(bin) = claude_bin() else {
        assert!(
            !required(),
            "required mode: claude CLI not found (check BOT_CLAUDE_BIN)"
        );
        eprintln!("skipping: claude CLI not found (set BOT_CLAUDE_CANARY=required to fail)");
        return;
    };
    let h = harness().await;
    let sink = deny_all_proxy().await;
    let first = tool_use_turn();
    let second = final_turn("TOOL-ROUND-TRIP-OK");
    let (a, b): (Vec<&[u8]>, Vec<&[u8]>) = (
        first.iter().map(Vec::as_slice).collect(),
        second.iter().map(Vec::as_slice).collect(),
    );
    h.upstream.respond_chunks(200, &a, End::Eof);
    h.upstream.respond_chunks(200, &b, End::Eof);
    h.runner.script(Ran::Done(JsonPayload(
        r#"{"content":[{"text":"ECHO:hello","type":"text"}],"isError":false}"#.into(),
    )));

    let (code, stdout, stderr) = run_claude(&bin, &h, &sink, &[], Duration::from_secs(60)).await;
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("TOOL-ROUND-TRIP-OK"), "{stdout}");
    let jobs = h.runner.jobs();
    assert_eq!(jobs.len(), 1, "{stdout}");
    assert_eq!(
        (
            jobs[0].call.0.as_str(),
            jobs[0].tool.id.0.as_str(),
            jobs[0].args.as_str()
        ),
        ("m-toolu_canary1", "echo.say", r#"{"text":"hello"}"#)
    );
    let sent = h.upstream.requests();
    assert_eq!(sent.len(), 2);
    for (endpoint, path, headers, body) in &sent {
        assert_eq!(
            (endpoint.as_str(), path.as_str()),
            ("api.main.example:443", "/v1/messages")
        );
        assert!(headers.contains(&("x-api-key".into(), KEY.into())));
        assert!(headers.iter().all(|(_, v)| !v.contains(&h.token)) && !body.contains(&h.token));
        assert!(body.contains("\"model\":\"model-large\"") && body.contains("\"stream\":true"));
        assert!(
            headers.iter().any(|(k, _)| k == "anthropic-version"),
            "{headers:?}"
        );
    }
    assert!(
        sent[0].3.contains("echo_say"),
        "model must see declared tools"
    );
    assert!(
        sent[1].3.contains("tool_result") && sent[1].3.contains("ECHO:hello"),
        "tool result must return to the next model request"
    );
    let stats = sink.stats();
    assert_eq!(
        stats.tunnels
            + stats.denied_policy
            + stats.denied_auth
            + stats.denied_address
            + stats.malformed
            + stats.failed,
        0,
        "{stats:?}"
    );
    sink.stop().await;
    h.front.stop().await;
}

#[tokio::test]
async fn the_real_cli_survives_a_blocked_stream_and_never_sees_the_key() {
    let Some(bin) = claude_bin() else {
        assert!(
            !required(),
            "required mode: claude CLI not found (check BOT_CLAUDE_BIN)"
        );
        return;
    };
    let h = harness().await;
    let sink = deny_all_proxy().await;
    let (head, tail) = KEY.split_at(9);
    let leak = [
        event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"model-large","content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        event(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{head}"}}}}"#
            ),
        ),
        event(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{tail}"}}}}"#
            ),
        ),
    ];
    let chunks: Vec<&[u8]> = leak.iter().map(Vec::as_slice).collect();
    for _ in 0..40 {
        h.upstream.respond_chunks(200, &chunks, End::Eof);
    }
    let (code, stdout, stderr) = run_claude(&bin, &h, &sink, &[], Duration::from_secs(20)).await;
    assert_ne!(
        code,
        Some(0),
        "blocked stream must not exit cleanly: {stdout}"
    );
    assert!(!h.upstream.requests().is_empty());
    assert!(
        !stdout.contains(tail)
            && !stderr.contains(tail)
            && !stdout.contains(KEY)
            && !stderr.contains(KEY),
        "provider key reached CLI: {stdout}"
    );
    assert!(
        !stdout.contains(head) && !stderr.contains(head),
        "provider key prefix reached CLI: {stdout}{stderr}"
    );
    assert!(h.runner.jobs().is_empty());
    sink.stop().await;
    h.front.stop().await;
}

#[tokio::test]
async fn the_real_cli_runs_a_tool_only_after_its_planned_position_was_recorded() {
    let Some(bin) = claude_bin() else {
        assert!(
            !required(),
            "required mode: claude CLI not found (check BOT_CLAUDE_BIN)"
        );
        return;
    };
    let h = harness_with(true).await;
    let sink = deny_all_proxy().await;
    let first = tool_use_turn();
    let second = final_turn("PLANNED-ROUND-TRIP-OK");
    let (a, b): (Vec<&[u8]>, Vec<&[u8]>) = (
        first.iter().map(Vec::as_slice).collect(),
        second.iter().map(Vec::as_slice).collect(),
    );
    h.upstream.respond_chunks(200, &a, End::Eof);
    h.upstream.respond_chunks(200, &b, End::Eof);
    h.runner.script(Ran::Done(JsonPayload(
        r#"{"content":[{"text":"ECHO:hello","type":"text"}],"isError":false}"#.into(),
    )));
    let (code, stdout, stderr) = run_claude(&bin, &h, &sink, &[], Duration::from_secs(60)).await;
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("PLANNED-ROUND-TRIP-OK"), "{stdout}");
    let jobs = h.runner.jobs();
    assert_eq!(jobs.len(), 1, "{stdout}");
    assert_eq!(
        (
            jobs[0].call.0.as_str(),
            jobs[0].tool.id.0.as_str(),
            jobs[0].args.as_str()
        ),
        ("cli-1-0", "echo.say", r#"{"text":"hello"}"#),
        "call ID must match logical turn position"
    );
    let recorded = h.ledger.step_keys("r1");
    for key in ["cli-resp-msg_t1", "cli-use-toolu_canary1", "cli-group-1"] {
        assert!(recorded.contains(&key.to_owned()), "{key}: {recorded:?}");
    }
    let sent = h.upstream.requests();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].3.contains("tool_result") && sent[1].3.contains("ECHO:hello"));
    assert!(sent.iter().all(|(_, _, headers, body)| {
        headers.iter().all(|(_, v)| !v.contains(&h.token)) && !body.contains(&h.token)
    }));
    sink.stop().await;
    h.front.stop().await;
}
