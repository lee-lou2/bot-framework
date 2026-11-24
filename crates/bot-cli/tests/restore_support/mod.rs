#![allow(dead_code)]
//! 재배포 복원 시험의 공용 도구: 한 시험의 저장소·호스트·소켓을 쥔 `Cluster`(프로필 폴더를 바꿔 가며 같은 저장소에 다시 띄운다), 관리 화면의 승인 카드 조회·결정,
//! 지침만 바꾼 판본·도구를 철회한 판본, 시작이 멈춰야 하는 `bot serve`.
//! 팀 시험 DB·`srt`·`python3`가 없으면 `Cluster::new`가 None이다(건너뜀).
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

/// 한 시험의 저장소·호스트·소켓. 프로필 폴더(판본)를 바꿔 가며 같은 저장소에 다시 띄운다.
pub struct Cluster {
    pub schema: Schema,
    pub dir: tempfile::TempDir,
    pub host: PathBuf,
    pub socket: PathBuf,
    pub token_file: PathBuf,
    pub installs: PathBuf,
}

/// 띄운 프로세스들(떨어지면 모두 끝난다)과 관리 화면 주소·비밀.
pub struct Running {
    pub _servers: Vec<Server>,
    pub admin: Option<(String, String)>,
}

impl Running {
    /// 띄운 프로세스가 모두 아직 돌고 있는가(시작하지 못한 역할은 곧바로 끝난다).
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

    /// 예제 프로필의 복사본(`edit`가 `personal` 폴더를 고친다) — 판본마다 폴더를 따로 둔다.
    pub fn profiles(&self, name: &str, edit: impl FnOnce(&Path)) -> PathBuf {
        let root = self.dir.path().join(name);
        copy_dir(&examples(), &root);
        edit(&root.join("personal"));
        root
    }

    /// `profiles`와 같되 바인딩이 없는 두 번째 프로필 `second`(지침 = `text`)가 더해진 폴더 — 집합에서 빠졌다 돌아오는 프로필을 흉내 낸다.
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

    /// gateway가 먼저 판본을 게시하고(`activate`면 교체) 나머지 역할은 같은 집합에 합류한다. `others`는 worker · scheduler · admin 중에서 고른다.
    pub fn start(&self, profiles: &Path, activate: bool, others: &[&str]) -> Running {
        let env = self.env(profiles);
        let env = refs(&env);
        let before = self.schema.active();
        let extra: &[&str] = if activate { &["--activate"] } else { &[] };
        let mut servers = vec![start_roles(&env, "gateway", extra, &self.socket)];
        wait_for("활성 판본이 게시되지 않았어요", || {
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
                    .expect("admin 시작");
                let stdout = child.stdout.take().unwrap();
                servers.push(Server(child));
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                        if line.contains("관리 화면: http://") {
                            let _ = tx.send(line);
                        }
                    }
                });
                let line = rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("관리 화면 주소 줄이 없어요");
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

/// 지침(AGENT.md)만 바꾼 새 판본: 승인 조건·도구·정책은 그대로다.
pub fn instructions_changed(personal: &Path) {
    append(
        personal.join("agents/main/AGENT.md"),
        "\n(재배포로 지침을 바꿨다)\n",
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

/// 승인 목록에서 대기 중인 카드 하나(관리 화면 API). 없으면 Null.
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

/// 소유자가 카드를 결정한다(화면이 본 인자 해시와 서버가 준 CSRF 토큰이 필요하다). (상태 코드, 본문)
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

/// 도구 하나를 철회한 새 판본: 선언에서 빼고(트리거가 그 도구를 쓰면 트리거도 없앤다) 정책은 그대로 둔다.
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

/// `bot serve`를 띄워 끝나기를 기다린다(시작하지 못하면 끝난다). 20초가 지나도 돌고 있으면 시작에 성공한 것이라 죽이고 `None`을 돌려준다.
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
        .expect("serve 시작");
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

/// 승인 카드가 열려 있는 상태에서 정지한 저장소: (클러스터, 옛 판본, 새 판본, 옛 판본의 프로필 판본, 옛 활성 판본)
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
            "산책 일정을 삭제해줘",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    wait_card("승인 카드가 열리지 않았어요", first.admin.as_ref().unwrap());
    let (r1, active) = (c.profile_revision(), c.schema.active());
    drop(first);
    Some((c, v1, v2, r1, active))
}

/// 시작이 멈춘 뒤에도 그대로여야 하는 것: 활성 집합 · 실행 시도 횟수 · 실행 상태 · 대기 카드.
pub fn fingerprint(c: &Cluster) -> String {
    format!(
        "{} | {} | {} | {}",
        c.schema.active(),
        c.q("select coalesce(sum(attempts), 0) from {s}.bot_runs"),
        c.q("select string_agg(id || ':' || state, ',' order by id) from {s}.bot_runs"),
        c.q("select string_agg(nonce || ':' || state, ',' order by nonce) from {s}.bot_approvals"),
    )
}
