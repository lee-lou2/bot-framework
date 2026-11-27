//! End-to-end observability tests using the real `bot` binary: secret checks for `admin.logs`/`admin.issues`,
//! `BOT_LOG_FORMAT=json` NDJSON output, and local mock delivery of OTLP traces, OTLP logs, and Sentry envelopes.
use crate::process_support::*;
use serde_json::Value;
use std::{
    io::Read,
    net::SocketAddr,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Clone, Debug)]
struct Captured {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn start_mock() -> (SocketAddr, Arc<Mutex<Vec<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store: Arc<Mutex<Vec<Captured>>> = Arc::default();
    let sink = store.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let sink = sink.clone();
            tokio::spawn(async move {
                let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
                let head_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut lines = head.lines();
                let path = lines
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .unwrap_or_default()
                    .to_owned();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let want = headers
                    .iter()
                    .find(|(k, _)| k == "content-length")
                    .and_then(|(_, v)| v.parse::<usize>().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + want {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                sink.lock().unwrap().push(Captured {
                    path,
                    headers,
                    body: buf[head_end..head_end + want].to_vec(),
                });
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    (addr, store)
}

#[test]
fn validate_and_doctor_check_host_observability_secrets_without_leaking_values() {
    let dir = tempfile::Builder::new()
        .prefix("obs")
        .tempdir_in("/tmp")
        .unwrap();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            "",
            r#", "providers": {"assistant": {"kind": "demo"}}, "admin": {
                "bind": "127.0.0.1:0",
                "trace": {"personal": {"endpoint": "https://cloud.langfuse.com/api/public/otel/v1/traces", "headers": {"Authorization": {"secret": "langfuse-auth"}}}},
                "logs": {"endpoint": "https://otlp.example.com/v1/logs", "headers": {"Authorization": {"secret": "otel-logs-key"}}, "level": "info"},
                "issues": {"kind": "sentry", "endpoint": "https://o0.ingest.sentry.io/api/1/envelope/", "key": {"secret": "sentry-key"}}
            }"#,
        ),
    )
    .unwrap();
    let profiles = examples();
    let base = [
        ("BOT_PROFILES", profiles.to_str().unwrap()),
        ("BOT_HOST_CONFIG", host.to_str().unwrap()),
    ];
    let missing = bot(&base, &["validate"]);
    let msg = text(&missing);
    assert!(!missing.status.success(), "{msg}");
    for expected in [
        "langfuse-auth",
        "otel-logs-key",
        "sentry-key",
        "BOT_SECRET__host__sentry_key",
    ] {
        assert!(msg.contains(expected), "missing {expected}:\n{msg}");
    }
    let secret_val = "SENTINEL-SECRET-OBS-VALUE-9182";
    let with_secrets = [
        ("BOT_PROFILES", profiles.to_str().unwrap()),
        ("BOT_HOST_CONFIG", host.to_str().unwrap()),
        (
            "BOT_DATABASE_URL",
            "postgres://postgres@127.0.0.1:5432/bot_demo",
        ),
        ("BOT_SECRET__personal__langfuse_auth", secret_val),
        ("BOT_SECRET__host__otel_logs_key", secret_val),
        ("BOT_SECRET__host__sentry_key", secret_val),
    ];
    let ok = bot(&with_secrets, &["validate"]);
    let ok_msg = text(&ok);
    assert!(ok.status.success(), "{ok_msg}");
    assert!(!ok_msg.contains(secret_val));

    let doc = bot(&with_secrets, &["doctor"]);
    let doc_msg = text(&doc);
    assert!(doc.status.success(), "{doc_msg}");
    assert!(
        doc_msg.lines().any(|line| {
            let low = line.to_ascii_lowercase();
            (low.contains("secret") || low.contains("credential")) && line.contains('3')
        }),
        "{doc_msg}"
    );
    assert!(!doc_msg.contains(secret_val));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_serve_and_chat_emit_json_logs_otlp_traces_otlp_logs_and_sentry_envelopes() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let (mock_addr, captured) = start_mock().await;
    let dir = tempfile::Builder::new()
        .prefix("obse2e")
        .tempdir_in("/tmp")
        .unwrap();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let fake_srt = dir.path().join("fake-srt");
    std::fs::write(
        &fake_srt,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"0.0.83\"; exit 0; fi\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_srt, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let work_root = dir.path().join("work");
    let installs = dir.path().join("installs");
    std::fs::create_dir_all(&work_root).unwrap();
    let host = dir.path().join("host.json");
    let socket = dir.path().join("run").join("g.sock");
    let token_file = dir.path().join("admin.token");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            "",
            &format!(
                r#", "providers": {{"assistant": {{"kind": "demo"}}}},
                "sandbox": {{"srt": {{"binary": "{}", "work_root": "{}"}}}},
                "admin": {{
                    "bind": "127.0.0.1:0",
                    "trace": {{"personal": {{
                        "name": "local-otlp",
                        "endpoint": "http://{mock_addr}/api/public/otel/v1/traces",
                        "headers": {{"Authorization": {{"secret": "otlp-auth"}}}}
                    }}}},
                    "logs": {{
                        "name": "local-logs",
                        "endpoint": "http://{mock_addr}/v1/logs",
                        "headers": {{"x-api-key": {{"secret": "logs-key"}}}},
                        "level": "info"
                    }},
                    "issues": {{
                        "kind": "sentry",
                        "name": "local-sentry",
                        "endpoint": "http://{mock_addr}/api/1/envelope/",
                        "key": {{"secret": "sentry-key"}},
                        "environment": "local-e2e"
                    }}
                }}"#,
                fake_srt.display(),
                work_root.display(),
            ),
        ),
    )
    .unwrap();

    let profiles = examples();
    let db_url = schema.url();
    let env = [
        ("BOT_PROFILES", profiles.to_str().unwrap()),
        ("BOT_HOST_CONFIG", host.to_str().unwrap()),
        ("BOT_DATABASE_URL", db_url.as_str()),
        ("BOT_SOCKET", socket.to_str().unwrap()),
        ("BOT_INSTALLS", installs.to_str().unwrap()),
        ("BOT_ADMIN_TOKEN_FILE", token_file.to_str().unwrap()),
        ("BOT_LOG", "info"),
        ("BOT_LOG_FORMAT", "json"),
        ("BOT_SECRET__personal__otlp_auth", "Basic bG9jYWw="),
        ("BOT_SECRET__host__logs_key", "logs-key-secret-e2e"),
        ("BOT_SECRET__host__sentry_key", "sentry-key-secret-e2e"),
    ];
    let mut child = Command::new(BOT)
        .args(["serve", "--role", "all"])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bot serve");

    let mut stderr_pipe = child.stderr.take().unwrap();
    let stderr_reader = std::thread::spawn(move || {
        let mut raw = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut raw);
        String::from_utf8_lossy(&raw).into_owned()
    });
    let mut stdout_pipe = child.stdout.take().unwrap();
    let stdout_reader = std::thread::spawn(move || {
        let mut raw = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut raw);
        String::from_utf8_lossy(&raw).into_owned()
    });

    for _ in 0..100 {
        if socket.exists() && !schema.active().is_empty() && token_file.exists() {
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let stderr = stderr_reader.join().unwrap();
            let stdout = stdout_reader.join().unwrap();
            panic!("bot serve exited early ({status}):\nstdout:\n{stdout}\nstderr:\n{stderr}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(socket.exists(), "gateway socket did not open");
    assert!(!schema.active().is_empty(), "active revision not published");
    assert!(token_file.exists(), "admin token file was not created");

    // 1) Successful chat emits OTLP traces (ingress + worker.run + worker.model).
    let chat = bot(
        &[
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_SOCKET", socket.to_str().unwrap()),
        ],
        &["chat", "--profile", "personal", "hello"],
    );
    assert!(chat.status.success(), "{}", text(&chat));

    // 2) Failing tool call (`fake-srt` exits 1) triggers an error span reported as a Sentry envelope.
    let fail_chat = bot(
        &[
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_SOCKET", socket.to_str().unwrap()),
        ],
        &[
            "chat",
            "--no-wait",
            "--profile",
            "personal",
            "Show today's agenda",
        ],
    );
    assert!(fail_chat.status.success(), "{}", text(&fail_chat));

    for _ in 0..200 {
        let ready = {
            let guard = captured.lock().unwrap();
            let has_trace = guard.iter().any(|r| r.path == "/api/public/otel/v1/traces");
            let has_log = guard.iter().any(|r| r.path == "/v1/logs");
            let has_sentry = guard.iter().any(|r| r.path == "/api/1/envelope/");
            has_trace && has_log && has_sentry
        };
        if ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Graceful SIGTERM flushes exporters before exit.
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let _ = child.wait();
    let stderr = stderr_reader.join().unwrap();
    let _stdout = stdout_reader.join().unwrap();

    // Structured log lines on stderr must all be valid NDJSON with zero secret leakage.
    let json_lines: Vec<Value> = stderr
        .lines()
        .filter(|l| l.starts_with('{'))
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect();
    assert!(!json_lines.is_empty(), "JSON logs are empty:\n{stderr}");
    assert!(
        json_lines
            .iter()
            .any(|v| v["level"] == "INFO" && v["target"].is_string() && v["ts_ms"].is_u64()),
        "structured log fields mismatch: {json_lines:?}"
    );
    for secret in ["logs-key-secret-e2e", "sentry-key-secret-e2e", "bG9jYWw="] {
        assert!(
            !stderr.contains(secret),
            "secret leaked to stderr: {secret}"
        );
    }

    let reqs = captured.lock().unwrap().clone();
    let trace_req = reqs
        .iter()
        .find(|r| r.path == "/api/public/otel/v1/traces")
        .expect("OTLP trace did not arrive");
    assert!(
        trace_req
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Basic bG9jYWw=")
    );
    assert!(
        trace_req
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/x-protobuf")
    );
    let log_req = reqs
        .iter()
        .find(|r| r.path == "/v1/logs")
        .expect("OTLP log did not arrive");
    assert!(
        log_req
            .headers
            .iter()
            .any(|(k, v)| k == "x-api-key" && v == "logs-key-secret-e2e")
    );
    assert!(
        log_req
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/x-protobuf")
    );
    let sentry_req = reqs
        .iter()
        .find(|r| r.path == "/api/1/envelope/")
        .expect("Sentry envelope did not arrive");
    assert!(
        sentry_req
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/x-sentry-envelope")
    );
    assert!(sentry_req.headers.iter().any(|(k, v)| k == "x-sentry-auth"
        && v.contains("sentry_version=7")
        && v.contains("sentry_key=sentry-key-secret-e2e")));
    let sentry_body = String::from_utf8(sentry_req.body.clone()).unwrap();
    let sentry_lines: Vec<&str> = sentry_body.lines().collect();
    assert_eq!(sentry_lines.len(), 3, "{sentry_body}");
    let sentry_item: Value = serde_json::from_str(sentry_lines[1]).unwrap();
    assert_eq!(sentry_item["length"], sentry_lines[2].len());
    let sentry_event: Value = serde_json::from_str(sentry_lines[2]).unwrap();
    assert_eq!(sentry_event["environment"], "local-e2e");
    assert!(!sentry_body.contains("sentry-key-secret-e2e"));
    assert!(!sentry_body.contains("logs-key-secret-e2e"));
}
