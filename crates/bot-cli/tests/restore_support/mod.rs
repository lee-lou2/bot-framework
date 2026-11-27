#![allow(dead_code)]
//! Shared harness for redeploy bundle-restore tests (multi-revision `Cluster`, Admin API approval
//! helpers, profile mutations, and startup-failure checks). Skips when PG, `srt`, or `python3` is missing.
use crate::process_support::*;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub struct Cluster {
    pub schema: Schema,
    pub dir: tempfile::TempDir,
    pub host: PathBuf,
    pub socket: PathBuf,
    pub token_file: PathBuf,
    pub installs: PathBuf,
}

pub struct Running {
    pub _servers: Vec<Server>,
    pub admin: Option<(String, String)>,
}

impl Running {
    pub fn alive(&mut self) -> bool {
        self._servers
            .iter_mut()
            .all(|server| server.0.try_wait().unwrap().is_none())
    }
}

impl Cluster {
    pub fn new() -> Option<Cluster> {
        let schema = Schema::create()?;
        if !have_srt_and_python() {
            return None;
        }
        let dir = tempfile::Builder::new()
            .prefix("g")
            .tempdir_in("/tmp")
            .unwrap();
        let server = std::fs::canonicalize(repo().join("examples/servers/demo-agenda")).unwrap();
        let host = dir.path().join("host.json");
        std::fs::write(
            &host,
            format!(
                r#"{{"version": 1,
                    "servers": {{"demo-agenda": {{"root": "{}", "command": ["bin/server", "--stdio"]}}}},
                    "models": {{"personal": ["assistant"], "second": ["assistant"]}},
                    "providers": {{"assistant": {{"kind": "demo"}}}},
                    "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": {}}}}}}}}},
                    "admin": {{"bind": "127.0.0.1:0"}}}}"#,
                server.display(),
                my_uid()
            ),
        )
        .unwrap();
        Some(Cluster {
            socket: dir.path().join("run").join("g.sock"),
            token_file: dir.path().join("admin.token"),
            installs: dir.path().join("installs"),
            host,
            dir,
            schema,
        })
    }

    pub fn profiles(&self, name: &str, edit: impl FnOnce(&Path)) -> PathBuf {
        let root = self.dir.path().join(name);
        copy_dir(&examples(), &root);
        edit(&root.join("personal"));
        root
    }

    /// Copies example profiles and adds an unbound `second` profile to test profile removal/re-addition.
    pub fn profiles_with_second(
        &self,
        name: &str,
        edit: impl FnOnce(&Path),
        text: &str,
    ) -> PathBuf {
        let root = self.profiles(name, edit);
        let second = root.join("second");
        std::fs::create_dir_all(second.join("agents/main")).unwrap();
        std::fs::write(
            second.join("profile.yaml"),
            "version: 1\nid: second\nmodel: assistant\nroles:\n  owner: [run]\nmembers:\n  - {identity: \"local:owner\", roles: [owner]}\nbindings: []\n",
        )
        .unwrap();
        std::fs::write(second.join("agents/main/AGENT.md"), text).unwrap();
        root
    }

    pub fn env(&self, profiles: &Path) -> Vec<(String, String)> {
        env_for(
            profiles,
            &self.schema.url(),
            &self.socket,
            &[
                ("BOT_HOST_CONFIG", self.host.to_str().unwrap()),
                ("BOT_ADMIN_TOKEN_FILE", self.token_file.to_str().unwrap()),
                ("BOT_INSTALLS", self.installs.to_str().unwrap()),
            ],
        )
    }

    /// Starts gateway first to publish/replace the active set, then starts `others` (`worker`/`scheduler`/`admin`).
    pub fn start(&self, profiles: &Path, activate: bool, others: &[&str]) -> Running {
        let env = self.env(profiles);
        let env = refs(&env);
        let before = self.schema.active();
        let extra: &[&str] = if activate { &["--activate"] } else { &[] };
        let mut servers = vec![start_roles(&env, "gateway", extra, &self.socket)];
        wait_for("active revision was not published", || {
            let now = self.schema.active();
            !now.is_empty() && (!activate || now != before)
        });
        let mut admin = None;
        for role in others {
            if *role == "admin" {
                let mut child = Command::new(BOT)
                    .args(["serve", "--role", "admin"])
                    .env_clear()
                    .envs(
                        std::env::vars()
                            .filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"),
                    )
                    .envs(env.iter().copied())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn admin");
                let stdout = child.stdout.take().unwrap();
                servers.push(Server(child));
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
                    .expect("missing Admin UI banner line");
                let addr = line
                    .split("http://")
                    .nth(1)
                    .and_then(|rest| rest.split("/ui").next())
                    .unwrap()
                    .to_owned();
                let token = std::fs::read_to_string(&self.token_file)
                    .unwrap()
                    .trim()
                    .to_owned();
                admin = Some((addr, token));
            } else {
                servers.push(spawn_role(&env, role));
            }
        }
        Running {
            _servers: servers,
            admin,
        }
    }

    pub fn q(&self, sql: &str) -> String {
        self.schema.query(sql)
    }

    pub fn profile_revision(&self) -> String {
        self.q("select revision from {s}.bot_active_profiles where profile = 'personal'")
    }
}

pub fn append(file: PathBuf, text: &str) {
    let mut body = std::fs::read_to_string(&file).unwrap();
    body.push_str(text);
    std::fs::write(file, body).unwrap();
}

pub fn instructions_changed(personal: &Path) {
    append(
        personal.join("agents/main/AGENT.md"),
        "\n(instructions updated for redeploy)\n",
    );
}

pub fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..300 {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what}");
}

pub fn pending_card(admin: &(String, String)) -> Value {
    let (status, body) = http_get(&admin.0, "/v1/admin/approvals", Some(&admin.1));
    assert_eq!(status, 200, "{body}");
    serde_json::from_str::<Value>(&body).unwrap_or(Value::Null)["items"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["state"] == "pending"))
        .cloned()
        .unwrap_or(Value::Null)
}

pub fn wait_card(what: &str, admin: &(String, String)) -> Value {
    for _ in 0..200 {
        let card = pending_card(admin);
        if !card.is_null() {
            return card;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what}");
}

pub fn decide(admin: &(String, String), card: &Value, allow: bool) -> (u16, String) {
    let (_, me) = http_get(&admin.0, "/v1/admin/me", Some(&admin.1));
    let me: Value = serde_json::from_str(&me).unwrap();
    http_post(
        &admin.0,
        &format!(
            "/v1/admin/approvals/{}/decide",
            card["nonce"].as_str().unwrap()
        ),
        &admin.1,
        me["csrf"].as_str().expect("csrf"),
        &format!(
            r#"{{"allow": {allow}, "seen": "{}"}}"#,
            card["args_sha256"].as_str().unwrap()
        ),
    )
}

/// Removes one agenda tool (and clears triggers if `list` is removed) while leaving policy unchanged.
pub fn tool_revoked(tool: &'static str) -> impl FnOnce(&Path) {
    move |personal: &Path| {
        let file = personal.join("agents/main/skills/agenda/tools.yaml");
        let text = std::fs::read_to_string(&file).unwrap();
        let (head, rest) = text.split_once("  - id: list\n").unwrap();
        let (list, delete) = rest.split_once("  - id: delete\n").unwrap();
        let kept = match tool {
            "delete" => format!("{head}  - id: list\n{list}"),
            _ => format!("{head}  - id: delete\n{delete}"),
        };
        std::fs::write(&file, kept).unwrap();
        if tool == "list" {
            std::fs::write(
                personal.join("agents/main/triggers.yaml"),
                "version: 1\ntriggers: []\n",
            )
            .unwrap();
        }
    }
}

/// Spawns `bot serve` expecting startup failure; kills it and returns `None` if still alive after 20s.
pub fn serve_that_must_stop(env: &[(&str, &str)], args: &[&str]) -> Option<(Option<i32>, String)> {
    let mut child = Command::new(BOT)
        .arg("serve")
        .args(args)
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    for _ in 0..200 {
        if child.try_wait().unwrap().is_some() {
            let out = child.wait_with_output().unwrap();
            return Some((out.status.code(), text(&out)));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Creates a store parked on a pending delete approval card: `(cluster, v1, v2, v1_rev, v1_active)`.
pub fn parked_with_a_pending_card() -> Option<(Cluster, PathBuf, PathBuf, String, String)> {
    let c = Cluster::new()?;
    let (v1, v2) = (
        c.profiles("v1", |_| {}),
        c.profiles("v2", instructions_changed),
    );
    let first = c.start(&v1, false, &["worker", "admin"]);
    let start = bot(
        &refs(&c.env(&v1)),
        &[
            "chat",
            "--no-wait",
            "--request-id",
            "parked-del",
            "Delete the walk event",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    wait_card("approval card did not open", first.admin.as_ref().unwrap());
    let (r1, active) = (c.profile_revision(), c.schema.active());
    drop(first);
    Some((c, v1, v2, r1, active))
}

/// Captures active set, attempt total, run states, and approval states to prove failed startups mutate nothing.
pub fn fingerprint(c: &Cluster) -> String {
    format!(
        "{} | {} | {} | {}",
        c.schema.active(),
        c.q("select coalesce(sum(attempts), 0) from {s}.bot_runs"),
        c.q("select string_agg(id || ':' || state, ',' order by id) from {s}.bot_runs"),
        c.q("select string_agg(nonce || ':' || state, ',' order by nonce) from {s}.bot_approvals"),
    )
}
