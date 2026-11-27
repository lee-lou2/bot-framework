//! Split-role acceptance tests launching `gateway`, `scheduler`, `worker`, and `admin` in separate processes over real PG and `srt`.
mod process_support;
use process_support::*;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn split_host(dir: &Path) -> PathBuf {
    let server = std::fs::canonicalize(repo().join("examples/servers/demo-agenda")).unwrap();
    let path = dir.join("host.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"version": 1,
                "servers": {{"demo-agenda": {{"root": "{}", "command": ["bin/server", "--stdio"]}}}},
                "models": {{"personal": ["assistant"]}},
                "providers": {{"assistant": {{"kind": "demo"}}}},
                "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": {}}}}}}}}},
                "admin": {{"bind": "127.0.0.1:0"}}}}"#,
            server.display(),
            my_uid()
        ),
    )
    .unwrap();
    path
}

struct Split {
    schema: Schema,
    env: Vec<(String, String)>,
    admin: String,
    token: String,
    _servers: Vec<Server>,
    _dir: tempfile::TempDir,
}

fn start_split() -> Option<Split> {
    let schema = Schema::create()?;
    if !have_srt_and_python() {
        return None;
    }
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let host = split_host(dir.path());
    let socket = dir.path().join("run").join("g.sock");
    let token_file = dir.path().join("admin.token");
    let installs = dir.path().join("installs");
    let profiles = examples();
    let url = schema.url();
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_ADMIN_TOKEN_FILE", token_file.to_str().unwrap()),
            ("BOT_INSTALLS", installs.to_str().unwrap()),
        ],
    );
    let refs = refs(&env);
    let gateway = start_gateway_roles(&refs, "gateway", &socket);
    wait_for("active revision was not published", || {
        !schema.active().is_empty()
    });
    let worker = spawn_role(&refs, "worker");
    let scheduler = spawn_role(&refs, "scheduler");
    let mut child = Command::new(BOT)
        .args(["serve", "--role", "admin"])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(refs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn admin");
    let stdout = child.stdout.take().unwrap();
    let admin = Server(child);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("Admin UI: http://") {
                let _ = tx.send(line);
            }
        }
    });
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("admin UI banner line missing");
    let addr = line
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split("/ui").next())
        .unwrap()
        .to_owned();
    let token = std::fs::read_to_string(&token_file)
        .unwrap()
        .trim()
        .to_owned();
    Some(Split {
        schema,
        env,
        admin: addr,
        token,
        _servers: vec![gateway, worker, scheduler, admin],
        _dir: dir,
    })
}

#[test]
fn starts_split_roles() {
    let Some(split) = start_split() else {
        return;
    };
    let env = refs(&split.env);
    let chat = bot(
        &env,
        &["chat", "--request-id", "split-list", "Show today's agenda"],
    );
    assert!(chat.status.success(), "{}", text(&chat));
    assert!(text(&chat).contains("Walk"), "{}", text(&chat));
    assert_eq!(
        split
            .schema
            .query("select count(*) from {s}.bot_runs where state = 'succeeded'"),
        "1"
    );
    assert_ne!(
        split
            .schema
            .query("select count(*) from {s}.bot_schedule_segments"),
        "0"
    );
}

#[test]
fn a_destructive_request_waits_for_the_owner_and_then_returns_a_simulated_receipt() {
    let Some(split) = start_split() else {
        return;
    };
    let env = refs(&split.env);
    let start = bot(
        &env,
        &[
            "chat",
            "--no-wait",
            "--request-id",
            "split-del",
            "Delete the walk event",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    let approvals = || -> Value {
        let (status, body) = http_get(&split.admin, "/v1/admin/approvals", Some(&split.token));
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap_or(Value::Null)
    };
    let mut pending = Value::Null;
    let mut last = String::new();
    for _ in 0..150 {
        let listed = approvals();
        last = listed.to_string();
        let found = listed["items"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["state"] == "pending"))
            .cloned();
        if let Some(item) = found {
            pending = item;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !pending.is_null(),
        "approval card did not open: list {last} / runs {}",
        split
            .schema
            .query("select string_agg(id || ':' || state || ':' || coalesce(last_error, ''), ' | ') from {s}.bot_runs")
    );
    let early = bot(
        &env,
        &["chat", "--attach", "--no-wait", "--request-id", "split-del"],
    );
    assert!(early.status.success(), "{}", text(&early));
    assert!(
        !text(&early).contains("sim-"),
        "deletion ran before approval: {}",
        text(&early)
    );
    let (_, me) = http_get(&split.admin, "/v1/admin/me", Some(&split.token));
    let me: Value = serde_json::from_str(&me).unwrap();
    let csrf = me["csrf"].as_str().expect("csrf").to_owned();
    let nonce = pending["nonce"].as_str().expect("nonce").to_owned();
    let seen = pending["args_sha256"]
        .as_str()
        .expect("args_sha256")
        .to_owned();
    let (status, body) = http_post(
        &split.admin,
        &format!("/v1/admin/approvals/{nonce}/decide"),
        &split.token,
        &csrf,
        &format!(r#"{{"allow": true, "seen": "{seen}"}}"#),
    );
    assert_eq!(status, 200, "{body}");
    let done = bot(&env, &["chat", "--attach", "--request-id", "split-del"]);
    assert!(done.status.success(), "{}", text(&done));
    assert!(
        text(&done).contains("Simulated") && text(&done).contains("virtual receipt sim-"),
        "{}",
        text(&done)
    );
    assert_eq!(
        split
            .schema
            .query("select count(*) from {s}.bot_runs where state = 'succeeded'"),
        "1"
    );
}

#[test]
#[ignore = "waits for next cron minute (up to 60s) — run with cargo test -- --ignored"]
fn runs_personal_demo() {
    let Some(schema) = Schema::create() else {
        return;
    };
    if !have_srt_and_python() {
        assert!(
            !Schema::required(),
            "runs_personal_demo requires srt and python3 when BOT_TEST_REQUIRE_DB=1"
        );
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let host = split_host(dir.path());
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let env = env_for(
        &examples(),
        &url,
        &socket,
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            (
                "BOT_ADMIN_TOKEN_FILE",
                dir.path().join("admin.token").to_str().unwrap(),
            ),
            (
                "BOT_INSTALLS",
                dir.path().join("installs").to_str().unwrap(),
            ),
        ],
    );
    let _all = start_gateway_roles(&refs(&env), "all", &socket);
    let env = refs(&env);
    let chat = bot(
        &env,
        &["chat", "--request-id", "demo-list", "Show today's agenda"],
    );
    assert!(chat.status.success(), "{}", text(&chat));
    assert!(text(&chat).contains("Walk"), "{}", text(&chat));
    // `morning` runs `agenda.list` directly every minute with zero model calls.
    let scheduled = || {
        schema.query(
            "select count(*) from {s}.bot_runs where trigger = 'morning' and state = 'succeeded'",
        )
    };
    let mut seen = String::new();
    for _ in 0..180 {
        seen = scheduled();
        if seen != "0" && !seen.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        seen != "0" && !seen.is_empty(),
        "scheduled run did not finish: {seen}"
    );
    assert_ne!(
        schema.query(
            "select count(*) from {s}.bot_runs where trigger = 'morning' and target like 'tool%'"
        ),
        "0"
    );
}
