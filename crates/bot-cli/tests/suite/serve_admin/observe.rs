//! OTLP span export tests for standalone gateways, withdrawal fencing, and request-path redaction.
use crate::process_support::*;

type Bodies = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

fn collector() -> (String, Bodies) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let (mut raw, mut chunk) = (Vec::new(), [0u8; 4096]);
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..n]);
                if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + length {
                        sink.lock()
                            .unwrap()
                            .push(raw[end + 4..end + 4 + length].to_vec());
                        break;
                    }
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    (format!("http://127.0.0.1:{port}/v1/traces"), bodies)
}

#[test]
fn a_standalone_gateway_exports_its_spans_to_the_configured_collector() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let (endpoint, bodies) = collector();
    let host = dir.path().join("host.json");
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            "",
            &format!(
                r#", "admin": {{"bind": "127.0.0.1:0", "trace": {{"personal": {{"endpoint": "{endpoint}"}}}}}}"#
            ),
        ),
    )
    .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let env = env_for(
        &examples(),
        &url,
        &socket,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    let server = start_gateway(&refs(&env), &[], &socket);
    let chat = bot(
        &refs(&env),
        &["chat", "--no-wait", "--request-id", "otlp-1", "Hello"],
    );
    assert!(chat.status.success(), "{}", text(&chat));
    wait_for("collector did not receive ingress span", || {
        bodies
            .lock()
            .unwrap()
            .iter()
            .any(|body| body.windows(11).any(|w| w == b"ingress cli"))
    });
    drop(server);
}

fn bodies_with(bodies: &Bodies, needle: &str) -> usize {
    bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|body| body.windows(needle.len()).any(|w| w == needle.as_bytes()))
        .count()
}

fn occurrences(bodies: &Bodies, needle: &str) -> usize {
    bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| {
            body.windows(needle.len())
                .filter(|w| *w == needle.as_bytes())
                .count()
        })
        .sum()
}

fn ingress_spans(bodies: &Bodies) -> usize {
    bodies_with(bodies, "ingress cli")
}

#[test]
fn withdrawing_collection_makes_the_old_process_a_stale_one_that_exports_nothing_more() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let profiles = examples();
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let (endpoint, bodies) = collector();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let write = |name: &str, extra: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, host_json(my_uid(), &pkg, "", extra)).unwrap();
        path
    };
    let collecting = write(
        "collecting.json",
        &format!(
            r#", "admin": {{"bind": "127.0.0.1:0", "trace": {{"personal": {{"endpoint": "{endpoint}", "content": true}}}}}}"#
        ),
    );
    let withdrawn = write("withdrawn.json", r#", "admin": {"bind": "127.0.0.1:0"}"#);
    let (socket_a, socket_b) = (
        dir.path().join("a").join("g.sock"),
        dir.path().join("b").join("g.sock"),
    );
    let url = schema.url();
    let env_a = env_for(
        &profiles,
        &url,
        &socket_a,
        &[("BOT_HOST_CONFIG", collecting.to_str().unwrap())],
    );
    let env_b = env_for(
        &profiles,
        &url,
        &socket_b,
        &[("BOT_HOST_CONFIG", withdrawn.to_str().unwrap())],
    );
    let a = start_gateway(&refs(&env_a), &[], &socket_a);
    wait_for("initial startup did not publish active revision", || {
        !schema.active().is_empty()
    });
    let operating = schema.active();

    let hello = bot(
        &refs(&env_a),
        &["chat", "--no-wait", "--request-id", "before-1", "Hello"],
    );
    assert!(hello.status.success(), "{}", text(&hello));
    wait_for("collector did not receive ingress span", || {
        ingress_spans(&bodies) >= 1
    });
    // Wait for span export batches from the control intake to quiesce before establishing baseline.
    let mut seen = ingress_spans(&bodies);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let now = ingress_spans(&bodies);
        if now == seen {
            break;
        }
        seen = now;
    }

    // Withdrawing tracing changes the activation fingerprint, marking process A stale.
    let b = start_gateway(&refs(&env_b), &["--activate"], &socket_b);
    wait_for(
        "withdrawing trace config did not change active revision",
        || schema.active() != operating,
    );

    let stale = bot(
        &refs(&env_a),
        &["chat", "--no-wait", "--request-id", "after-1", "Hello"],
    );
    assert_eq!(stale.status.code(), Some(3), "{}", text(&stale));
    std::thread::sleep(std::time::Duration::from_secs(4));
    assert_eq!(
        ingress_spans(&bodies),
        seen,
        "stale process exported additional spans after collection withdrawal"
    );
    drop((a, b));
}

const ISSUER: &str = "https://issuer.example";
const INBOX_KEY: &str = "0123456789abcdef0123456789abcdef";

fn raw_request(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> u16 {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect to HTTP");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(text.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    String::from_utf8_lossy(&raw)
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

/// Because the trace policy does not pseudonymize `http.route` even with `content: true, ids: "plain"`,
/// the gateway must never record raw request paths on spans.
#[test]
fn identifiers_and_secrets_carried_in_a_request_path_never_reach_the_collector() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let profiles = dir.path().join("profiles");
    with_inbox(&profiles);
    let (endpoint, bodies) = collector();
    let port = free_port();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            &format!(
                r#", "http": {{"listen": "127.0.0.1:{port}", "endpoints": {{"inbox": {{"issuer": "{ISSUER}", "audience": "bot", "key": "inbox-key"}}}}}}"#
            ),
            &format!(
                r#", "admin": {{"bind": "127.0.0.1:0", "trace": {{"personal": {{"endpoint": "{endpoint}", "content": true, "ids": "plain"}}}}}}"#
            ),
        ),
    )
    .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_SECRET__personal__inbox_key", INBOX_KEY),
        ],
    );
    let server = start_gateway(&refs(&env), &[], &socket);
    wait_for("initial startup did not publish active revision", || {
        !schema.active().is_empty()
    });
    wait_for("HTTP entrance did not open", || {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    });

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let token = bot_gateway::http::token::sign(
        &serde_json::json!({"iss": ISSUER, "aud": "bot", "sub": "svc", "exp": now + 3600}),
        INBOX_KEY.as_bytes(),
    );
    let auth = format!("Bearer {token}");
    let send = |method: &str, path: &str, key: &str, body: &str| {
        raw_request(
            port,
            method,
            path,
            &[
                ("Authorization", &auth),
                ("Idempotency-Key", key),
                ("Content-Type", "application/json"),
            ],
            body,
        )
    };

    assert_eq!(send("POST", "/inbox/notify", "control-1", "{}"), 202);

    const ID: &str = "usr-4242-ZQJXKW";
    const SECRET: &str = "sk-live-Qm9x7vT2pLdR";
    let hostile = [
        ("POST", format!("/inbox/users/{ID}/keys/{SECRET}")),
        ("PUT", "/inbox/notify".to_owned()),
        ("POST", format!("/inbox/Notify/{SECRET}")),
        ("POST", format!("/inbox/notify%2F{ID}%2F{SECRET}")),
        ("GET", format!("/inbox/notify/{ID}?token={SECRET}")),
    ];
    for (n, (method, path)) in hostile.iter().enumerate() {
        assert_eq!(
            send(method, path, &format!("hostile-{n}"), ""),
            404,
            "{method} {path}"
        );
    }

    let expected = 1 + hostile.len();
    wait_for("collector did not receive all ingress spans", || {
        occurrences(&bodies, "ingress http") >= expected
    });
    std::thread::sleep(std::time::Duration::from_millis(2500));

    for needle in [
        ID,
        SECRET,
        "ZQJXKW",
        "Qm9x7vT2pLdR",
        "usr-4242",
        "sk-live",
        "/keys/",
        "Notify",
    ] {
        assert_eq!(
            bodies_with(&bodies, needle),
            0,
            "path token `{needle}` leaked to collector"
        );
    }
    assert!(
        bodies_with(&bodies, "unmatched") >= 1,
        "missing fixed route label for unmatched requests"
    );
    assert!(
        bodies_with(&bodies, "_OTHER") >= 1,
        "missing fixed method label for unmatched requests"
    );
    assert!(
        bodies_with(&bodies, "/notify") >= 1,
        "missing declared route label for matched request"
    );
    drop(server);
}
