//! Real HTTPS (TLS) transport verification for OTLP traces, OTLP logs, and Sentry envelopes, including redirect refusal.

use bot_admin::{
    issues::{IssueConfig, IssueExporter, IssueKind, IssueOptions},
    logs::{LogConfig, LogEntry, LogExporter, LogLevel},
    obs::{HostHeaderValue, Options, SecretRef},
    trace::{
        Backend, Exporter, HeaderValue, Ids, ProfileTrace, Redactor, SpanIds, SpanRecord, Tracer,
    },
};
use bot_core::{
    id::{Attempt, Generation, ProfileId, RunId, SecretName},
    issue::{IssueEvent, IssueLevel, IssueReporter},
    port::{Error, ProfileSecrets, Reply, SecretStore, SecretValue},
    trace::now_nano,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

struct OneSecret(&'static str, &'static str, &'static str);
struct OneHandle(Option<(&'static str, String)>);

impl SecretStore for OneSecret {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let val = (profile.0 == self.0).then(|| (self.1, self.2.to_owned()));
        Box::pin(async move { Ok(Box::new(OneHandle(val)) as Box<dyn ProfileSecrets>) })
    }
}

impl ProfileSecrets for OneHandle {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        let v = self
            .0
            .as_ref()
            .filter(|(expected, _)| name.as_str() == *expected)
            .map(|(_, value)| value.clone());
        Box::pin(async move { v.map(|s| SecretValue(s.into_bytes())).ok_or(Error::Missing) })
    }
}

fn fast() -> Options {
    Options {
        queue: 64,
        batch: 16,
        interval: Duration::from_millis(20),
        timeout: Duration::from_millis(2000),
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        trip_after: 2,
    }
}

fn run_openssl(args: &[&str]) {
    let status = Command::new("openssl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run openssl");
    assert!(status.success(), "openssl failed: {args:?}");
}

struct TlsServerGuard {
    child: std::process::Child,
    dir: std::path::PathBuf,
}

impl Drop for TlsServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Generates a local CA + server cert pair (`localhost`, `127.0.0.1`) accepted by rustls-webpki.
fn make_tls_chain(dir: &Path) -> (Vec<u8>, std::path::PathBuf, std::path::PathBuf) {
    let ca_key = dir.join("ca.key");
    let ca_cert = dir.join("ca.pem");
    let srv_key = dir.join("srv.key");
    let srv_csr = dir.join("srv.csr");
    let srv_cert = dir.join("srv.pem");
    let srv_ext = dir.join("srv.ext");
    std::fs::write(
        &srv_ext,
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n",
    )
    .unwrap();
    run_openssl(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        ca_key.to_str().unwrap(),
        "-out",
        ca_cert.to_str().unwrap(),
        "-days",
        "1",
        "-subj",
        "/CN=bot-test-ca",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign",
    ]);
    run_openssl(&[
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        srv_key.to_str().unwrap(),
        "-out",
        srv_csr.to_str().unwrap(),
        "-subj",
        "/CN=localhost",
    ]);
    run_openssl(&[
        "x509",
        "-req",
        "-in",
        srv_csr.to_str().unwrap(),
        "-CA",
        ca_cert.to_str().unwrap(),
        "-CAkey",
        ca_key.to_str().unwrap(),
        "-CAcreateserial",
        "-out",
        srv_cert.to_str().unwrap(),
        "-days",
        "1",
        "-extfile",
        srv_ext.to_str().unwrap(),
    ]);
    let ca_pem = std::fs::read(&ca_cert).unwrap();
    (ca_pem, srv_cert, srv_key)
}

#[tokio::test]
async fn real_https_tls_transport_delivers_traces_logs_and_sentry_and_refuses_redirects() {
    let dir = std::env::temp_dir().join(format!("bot-tls-test-{}", now_nano()));
    std::fs::create_dir_all(&dir).unwrap();
    let (ca_pem, srv_cert, srv_key) = make_tls_chain(&dir);
    let port_file = dir.join("port.txt");
    let log_file = dir.join("requests.jsonl");

    let script = dir.join("server.py");
    std::fs::write(
        &script,
        r#"import http.server, json, ssl, sys, threading
cert, key, port_file, log_file = sys.argv[1:5]
lock = threading.Lock()
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(n)
        with lock:
            with open(log_file, "a") as f:
                f.write(json.dumps({
                    "path": self.path,
                    "ct": self.headers.get("Content-Type", ""),
                    "auth": self.headers.get("Authorization", ""),
                    "sentry": self.headers.get("X-Sentry-Auth", ""),
                    "len": len(body),
                }) + "\n")
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "https://127.0.0.1/other")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()
    def log_message(self, *args):
        pass
srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(certfile=cert, keyfile=key)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
with open(port_file, "w") as f:
    f.write(str(srv.server_address[1]))
srv.serve_forever()
"#,
    )
    .unwrap();
    let child = Command::new("python3")
        .args([
            script.to_str().unwrap(),
            srv_cert.to_str().unwrap(),
            srv_key.to_str().unwrap(),
            port_file.to_str().unwrap(),
            log_file.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start local HTTPS mock server");
    let mut guard = TlsServerGuard { child, dir };

    let mut port = None;
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&port_file)
            && let Ok(p) = text.trim().parse::<u16>()
        {
            port = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let port = port.expect("timed out waiting for HTTPS server port");

    let trace_cfg = BTreeMap::from([
        (
            ProfileId("alpha".into()),
            ProfileTrace {
                backend: Some(Backend {
                    name: "langfuse-tls".into(),
                    endpoint: format!("https://localhost:{port}/api/public/otel/v1/traces"),
                    headers: vec![(
                        "Authorization".into(),
                        HeaderValue::Plain("Basic dGVzdA==".into()),
                    )],
                    link: None,
                    remote: false,
                }),
                ids: Ids::Plain,
                content: false,
                sample: 1.0,
            },
        ),
        (
            ProfileId("redir".into()),
            ProfileTrace {
                backend: Some(Backend {
                    name: "redir".into(),
                    endpoint: format!("https://127.0.0.1:{port}/redirect"),
                    headers: Vec::new(),
                    link: None,
                    remote: false,
                }),
                ids: Ids::Plain,
                content: false,
                sample: 1.0,
            },
        ),
    ]);
    let traces =
        Exporter::start_with_root_pem(trace_cfg, None, Redactor::default(), fast(), &ca_pem)
            .expect("start HTTPS trace exporter");
    for p in ["alpha", "redir"] {
        let a = Attempt {
            profile: ProfileId(p.into()),
            run: RunId("r-tls".into()),
            claim: None,
            generation: Generation(1),
        };
        traces.record(SpanRecord::start(
            &a.profile,
            "worker.run",
            SpanIds::attempt(&a),
        ));
    }
    assert!(traces.flush(Duration::from_secs(5)).await);
    let a_stat = traces.stats(&ProfileId("alpha".into())).unwrap();
    assert_eq!(a_stat.sent, 1, "alpha stats: {a_stat:?}");
    let r_stat = traces.stats(&ProfileId("redir".into())).unwrap();
    assert_eq!(
        (r_stat.sent, r_stat.failed, r_stat.last_error),
        (0, 1, Some("status"))
    );

    let logs = LogExporter::start_with_root_pem(
        Some(LogConfig {
            name: "logs-tls".into(),
            endpoint: format!("https://127.0.0.1:{port}/v1/logs"),
            headers: vec![(
                "Authorization".into(),
                HostHeaderValue::Plain("Bearer tls".into()),
            )],
            level: LogLevel::Info,
            remote: false,
        }),
        None,
        Redactor::default(),
        fast(),
        &ca_pem,
    )
    .expect("start HTTPS log exporter");
    logs.record(LogEntry {
        timestamp_nano: now_nano(),
        level: LogLevel::Info,
        target: "bot_cli::test".into(),
        message: "tls log ok".into(),
        fields: Vec::new(),
        trace_id: None,
        span_id: None,
    });
    assert!(logs.flush(Duration::from_secs(5)).await);
    assert_eq!(logs.stats().sent, 1, "{:?}", logs.stats());

    let secrets: Arc<dyn SecretStore> = Arc::new(OneSecret("host", "sentry-key", "tls_sentry_key"));
    let issues = IssueExporter::start_with_root_pem(
        Some(IssueConfig {
            kind: IssueKind::Sentry,
            name: "sentry-tls".into(),
            endpoint: format!("https://127.0.0.1:{port}/api/1/envelope/"),
            key: Some(SecretRef::host(SecretName::parse("sentry-key").unwrap())),
            environment: Some("tls-test".into()),
            release: None,
            sample: 1.0,
            remote: false,
        }),
        Some(secrets),
        Redactor::default(),
        IssueOptions {
            runtime: fast(),
            cooldown: Duration::ZERO,
        },
        &ca_pem,
    )
    .expect("start HTTPS issue exporter");
    issues.report(IssueEvent::new(
        IssueLevel::Error,
        "tls_error",
        "tls issue ok",
        "bot_cli::test",
    ));
    assert!(issues.flush(Duration::from_secs(5)).await);
    assert_eq!(issues.stats().sent, 1, "{:?}", issues.stats());

    let _ = guard.child.kill();
    let _ = guard.child.wait();
    let lines: Vec<Value> = std::fs::read_to_string(&log_file)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        lines
            .iter()
            .any(|r| r["path"] == "/api/public/otel/v1/traces"
                && r["ct"] == "application/x-protobuf"
                && r["auth"] == "Basic dGVzdA==")
    );
    assert!(
        lines
            .iter()
            .any(|r| r["path"] == "/v1/logs" && r["ct"] == "application/x-protobuf")
    );
    assert!(lines.iter().any(|r| {
        r["path"] == "/api/1/envelope/"
            && r["ct"] == "application/x-sentry-envelope"
            && r["sentry"]
                .as_str()
                .is_some_and(|s| s.contains("sentry_key=tls_sentry_key"))
    }));
}
