//! Shared utilities for real `bot` subprocess tests: binary invocation, PID-scoped PG schemas, role servers, and raw HTTP helpers.
//! Tests requiring PG skip when `BOT_TEST_DATABASE_URL` is unset unless `BOT_TEST_REQUIRE_DB=1`.
#![allow(dead_code)]
use std::{
    io::{BufRead, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

pub const BOT: &str = env!("CARGO_BIN_EXE_bot");

/// Per-process copy of `examples/profiles/personal` only, because custom test host configs map only `personal`.
pub fn examples() -> PathBuf {
    static COPY: OnceLock<PathBuf> = OnceLock::new();
    COPY.get_or_init(|| {
        let dir =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("examples-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles/personal");
        copy_dir(&source, &dir.join("personal"));
        dir
    })
    .clone()
}

pub fn bot(envs: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    Command::new(BOT)
        .args(args)
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(envs.iter().copied())
        .output()
        .expect("run bot")
}

pub fn text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub struct Schema {
    pub base: String,
    pub name: String,
}

impl Schema {
    pub fn required() -> bool {
        std::env::var("BOT_TEST_REQUIRE_DB").is_ok_and(|value| value == "1")
    }

    /// Creates a PID-scoped temporary PG schema; returns `None` if DB is absent unless `BOT_TEST_REQUIRE_DB=1`.
    pub fn create() -> Option<Self> {
        let required = Self::required();
        let fail = |why: &str| -> Option<Self> {
            if required {
                panic!("BOT_TEST_REQUIRE_DB=1 but test DB is unavailable: {why}");
            }
            None
        };
        let Ok(base) = std::env::var("BOT_TEST_DATABASE_URL") else {
            return fail("BOT_TEST_DATABASE_URL is not set");
        };
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "gw_e2e_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        match Command::new("psql")
            .args([&base, "-qAtc", &format!("create schema {name}")])
            .stdout(Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => Some(Self { base, name }),
            Ok(output) => fail(&format!(
                "failed to create schema ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(error) => fail(&format!("failed to run psql: {error}")),
        }
    }

    pub fn url(&self) -> String {
        format!("{}?options=-c%20search_path%3D{}", self.base, self.name)
    }

    pub fn active(&self) -> String {
        let out = Command::new("psql")
            .args([
                &self.base,
                "-Atc",
                &format!(
                    "select coalesce(max(revision), '') from {}.bot_active",
                    self.name
                ),
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    pub fn query(&self, sql: &str) -> String {
        let out = Command::new("psql")
            .args([&self.base, "-Atc", &sql.replace("{s}", &self.name)])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Passes SQL over stdin (`-f -`) so large hex bundles do not hit Linux `MAX_ARG_STRLEN` (128 KiB).
    pub fn exec(&self, sql: &str) -> String {
        let mut rendered = sql.replace("{s}", &self.name);
        if !rendered.trim_end().ends_with(';') {
            rendered.push(';');
        }
        rendered.push('\n');
        let mut child = Command::new("psql")
            .args([&self.base, "-At", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(rendered.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }

    pub fn tables(&self) -> String {
        let out = Command::new("psql")
            .args([
                &self.base,
                "-Atc",
                &format!(
                    "select count(*) from information_schema.tables where table_schema = '{}'",
                    self.name
                ),
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    pub fn count(&self, table: &str) -> String {
        let out = Command::new("psql")
            .args([
                &self.base,
                "-Atc",
                &format!("select count(*) from {}.{table}", self.name),
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }
}

impl Drop for Schema {
    fn drop(&mut self) {
        let _ = Command::new("psql")
            .args([
                &self.base,
                "-qAtc",
                &format!("drop schema {} cascade", self.name),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Holds a PostgreSQL `FOR UPDATE` row lock on `bot_active` until dropped.
pub struct ActiveRowLock {
    connection: Child,
}

impl Schema {
    pub fn hold_active_row(&self) -> ActiveRowLock {
        let connection = Command::new("psql")
            .env("PGCONNECT_TIMEOUT", "10")
            .env("PGAPPNAME", format!("lock-{}", self.name))
            .args(["-XqAtw", "-v", "ON_ERROR_STOP=1", &self.base])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("psql");
        let mut lock = ActiveRowLock { connection };
        let input = lock.connection.stdin.as_mut().expect("psql stdin");
        writeln!(
            input,
            "begin; set local lock_timeout = '10s'; select coalesce((select 1 from {}.bot_active where singleton for update), 0);",
            self.name
        ).expect("send lock query");
        let output = lock.connection.stdout.take().expect("psql stdout");
        let mut reply = String::new();
        std::io::BufReader::new(output)
            .read_line(&mut reply)
            .expect("read lock reply");
        assert_eq!(reply.trim(), "1", "expected to lock active revision row");
        lock
    }
}

impl Drop for ActiveRowLock {
    fn drop(&mut self) {
        // Closing stdin sends EOF to psql so PostgreSQL rolls back the uncommitted transaction.
        drop(self.connection.stdin.take());
        let _ = self.connection.wait();
    }
}

pub struct Server(pub Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

pub fn start_gateway(envs: &[(&str, &str)], extra: &[&str], socket: &Path) -> Server {
    let server = Server(
        Command::new(BOT)
            .args(["serve", "--role", "gateway"])
            .args(extra)
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
            .envs(envs.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn serve"),
    );
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(socket.exists(), "gateway socket did not open");
    server
}

pub fn start_gateway_roles(envs: &[(&str, &str)], roles: &str, socket: &Path) -> Server {
    start_roles(envs, roles, &[], socket)
}

pub fn start_roles(envs: &[(&str, &str)], roles: &str, extra: &[&str], socket: &Path) -> Server {
    let server = Server(
        Command::new(BOT)
            .args(["serve", "--role", roles])
            .args(extra)
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
            .envs(envs.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn serve"),
    );
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(socket.exists(), "gateway socket did not open");
    server
}

pub fn spawn_role(envs: &[(&str, &str)], role: &str) -> Server {
    Server(
        Command::new(BOT)
            .args(["serve", "--role", role])
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
            .envs(envs.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn serve"),
    )
}

pub fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Checks for `srt` and `python3`; panics if `BOT_SRT_CANARY=required`, otherwise emits a `skipped:` line.
pub fn have_srt_and_python() -> bool {
    let ok = which("srt").is_some() && which("python3").is_some();
    if !ok {
        assert!(
            std::env::var("BOT_SRT_CANARY").as_deref() != Ok("required"),
            "BOT_SRT_CANARY=required: srt and python3 are required"
        );
        eprintln!("skipped: srt or python3 not found");
    }
    ok
}

pub fn have_srt() -> bool {
    let ok = which("srt").is_some();
    if !ok {
        assert!(
            std::env::var("BOT_SRT_CANARY").as_deref() != Ok("required"),
            "BOT_SRT_CANARY=required: srt is required"
        );
        eprintln!("skipped: srt not found");
    }
    ok
}

/// Polls up to 10s until `ready` holds (socket creation precedes active-revision publication).
pub fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..100 {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what}");
}

pub fn env_for(
    profiles: &Path,
    url: &str,
    socket: &Path,
    extra: &[(&str, &str)],
) -> Vec<(String, String)> {
    let mut pairs = vec![
        (
            "BOT_PROFILES".to_owned(),
            profiles.to_str().unwrap().to_owned(),
        ),
        ("BOT_DATABASE_URL".to_owned(), url.to_owned()),
        ("BOT_SOCKET".to_owned(), socket.to_str().unwrap().to_owned()),
    ];
    pairs.extend(
        extra
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    pairs
}

pub fn refs(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
    pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

pub fn http_get(addr: &str, path: &str, token: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect to admin server");
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_owned())
}

pub fn my_uid() -> u32 {
    let out = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .unwrap()
        .stdout;
    String::from_utf8_lossy(&out).trim().parse().unwrap()
}

pub fn package(dir: &Path, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let root = dir.join("pkg");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let server = root.join("bin/server");
    std::fs::write(&server, script).unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    root
}

pub fn host_json(uid: u32, server_root: &Path, extra_gateway: &str, extra_top: &str) -> String {
    format!(
        r#"{{"version": 1,
            "servers": {{"demo-agenda": {{"root": "{root}", "command": ["bin/server", "--stdio"]}}}},
            "models": {{"personal": ["assistant"]}},
            "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": {uid}}}}}}}{extra_gateway}}}{extra_top}}}"#,
        root = server_root.display()
    )
}

pub fn http_post(addr: &str, path: &str, token: &str, csrf: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect to admin server");
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nx-bot-csrf: {csrf}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_owned())
}

/// Copies the example profile to `to` and adds HTTP binding `inbox` (`http:inbox/svc`) with a `POST /notify` trigger.
pub fn with_inbox(to: &Path) {
    copy_dir(&examples(), to);
    let yaml_path = to.join("personal/profile.yaml");
    let yaml = std::fs::read_to_string(&yaml_path).unwrap();
    let desk = "  - {id: desk, adapter: cli, account: owner, agent: main}";
    let owner = "  - {identity: \"local:owner\", roles: [owner]}";
    assert!(
        yaml.contains(desk) && yaml.contains(owner),
        "example profile format changed"
    );
    let yaml = yaml
        .replace(
            desk,
            &format!("{desk}\n  - {{id: inbox, adapter: http, endpoint: inbox, agent: main}}"),
        )
        .replace(
            owner,
            &format!("{owner}\n  - {{identity: \"http:inbox/svc\", roles: [reader]}}"),
        );
    std::fs::write(&yaml_path, yaml).unwrap();
    let triggers_path = to.join("personal/agents/main/triggers.yaml");
    let mut triggers = std::fs::read_to_string(&triggers_path).unwrap();
    triggers.push_str(
        "  - id: notify\n    \"on\": {http: {method: POST, path: /notify}, binding: inbox}\n    target: {tool: agenda.list, args: {}}\n",
    );
    std::fs::write(&triggers_path, triggers).unwrap();
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
