//! Admin role and observability process tests: operator tokens are delivered only via TTY or 0600 file,
//! stale-revision processes return empty 503 responses, and shutdown cleans up the token file without auditing secrets.
#[path = "serve_admin/observe.rs"]
mod observe;
use crate::process_support::*;
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    time::Duration,
};

#[test]
fn the_admin_role_opens_an_authenticated_screen_and_never_puts_the_secret_on_a_pipe() {
    use std::os::unix::fs::PermissionsExt;
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let profiles = examples();
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let host = dir.path().join("host.json");
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    std::fs::write(
        &host,
        host_json(my_uid(), &pkg, "", r#", "admin": {"bind": "127.0.0.1:0"}"#),
    )
    .unwrap();
    let token_path = dir.path().join("admin.token");
    let url = schema.url();
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_ADMIN_TOKEN_FILE", token_path.to_str().unwrap()),
        ],
    );
    let mut child = Command::new(BOT)
        .args(["serve", "--role", "gateway,admin"])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(refs(&env))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn serve");
    let stdout = child.stdout.take().unwrap();
    let mut server = Server(child);
    // Keep draining stdout until exit so the pipe never blocks and we can assert no token was printed.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains("Admin UI: http://") {
                    let _ = tx.send(line.clone());
                }
                seen.lock().unwrap().push(line);
            }
        })
    };
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("missing Admin UI URL line");
    let addr = line
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split("/ui").next())
        .expect("admin address")
        .to_owned();
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(token.len(), 64);
    assert_eq!(
        std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        seen.lock().unwrap().iter().all(|l| !l.contains(&token)),
        "operator token leaked to piped stdout: {:?}",
        seen.lock().unwrap()
    );

    assert_eq!(http_get(&addr, "/v1/admin/me", None).0, 401);
    assert_eq!(http_get(&addr, "/v1/admin/me", Some("wrong")).0, 401);
    let (status, body) = http_get(&addr, "/v1/admin/me", Some(&token));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("local:owner"), "{body}");
    assert_eq!(http_get(&addr, "/v1/admin/status", Some(&token)).0, 200);

    let (_, map) = http_get(&addr, "/v1/admin/map", Some(&token));
    assert!(map.contains("personal"), "{map}");
    assert_eq!(http_get(&addr, "/v1/admin/approvals", Some(&token)).0, 200);
    // Simulate another process replacing the active revision: this stale instance must fail closed with empty 503s.
    schema.query(
        "update {s}.bot_active set revision = 'replaced-by-another-process' where singleton",
    );
    for path in [
        "/v1/admin/me",
        "/v1/admin/map",
        "/v1/admin/approvals",
        "/v1/admin/status",
    ] {
        let (status, body) = http_get(&addr, path, Some(&token));
        assert_eq!(status, 503, "{path}: {body}");
        assert!(
            !body.contains("personal") && !body.contains("local:owner"),
            "{path}: stale content leaked: {body}"
        );
    }
    // Auth rejection precedes fence checks (401), while static `/ui` stays readable (200).
    assert_eq!(http_get(&addr, "/v1/admin/me", None).0, 401);
    assert_eq!(http_get(&addr, "/ui", Some(&token)).0, 200);

    let pid = server.0.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    wait_for("server did not exit", || {
        server.0.try_wait().ok().flatten().is_some()
    });
    wait_for("token file was not removed", || !token_path.exists());
    reader.join().unwrap();
    assert!(seen.lock().unwrap().iter().all(|l| !l.contains(&token)));
    let dump = Command::new("psql")
        .args([
            &schema.base,
            "-Atc",
            &format!(
                "select count(*) from {0}.bot_audit a where a::text like '%{1}%'",
                schema.name, token
            ),
        ])
        .output()
        .unwrap();
    let count = String::from_utf8_lossy(&dump.stdout).trim().to_owned();
    assert!(
        count == "0" || count.is_empty(),
        "operator token leaked into audit table: {count}"
    );
}

#[test]
fn an_admin_start_without_a_terminal_or_a_token_file_stops_before_publishing_anything() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let host = dir.path().join("host.json");
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    std::fs::write(
        &host,
        host_json(my_uid(), &pkg, "", r#", "admin": {"bind": "127.0.0.1:0"}"#),
    )
    .unwrap();
    let url = schema.url();
    let env = env_for(
        &examples(),
        &url,
        &dir.path().join("run").join("g.sock"),
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    // `bot()` pipes stdout, so it is non-interactive.
    let started = bot(&refs(&env), &["serve", "--role", "admin", "--activate"]);
    assert!(!started.status.success(), "{}", text(&started));
    assert!(
        text(&started).contains("BOT_ADMIN_TOKEN_FILE"),
        "{}",
        text(&started)
    );
    assert_eq!(
        schema.active(),
        "",
        "start without token delivery must not publish a revision"
    );
}
