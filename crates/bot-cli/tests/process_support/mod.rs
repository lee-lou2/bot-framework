//! 실제 `bot` 프로세스 시험의 공용 도구: 실행 파일 호출, 팀 시험 DB의 내 PID 스키마, 서버 프로세스, 원시 HTTP 클라이언트.
//! 팀 시험 DB(`BOT_TEST_DATABASE_URL`, 예: postgres://<사용자>@localhost:<포트>/bot_test)와 `psql`이 있어야 하고,
//! 없으면 DB가 필요한 시험은 건너뛴다. 시험 스키마는 이 프로세스의 PID가 붙은 것만 만들고 지운다.
#![allow(dead_code)]
use std::{
    io::{BufRead, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

pub const BOT: &str = env!("CARGO_BIN_EXE_bot");

pub fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles")
}

pub fn bot(envs: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    Command::new(BOT)
        .args(args)
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(envs.iter().copied())
        .output()
        .expect("bot 실행")
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
    /// 시험 DB가 필수인가(`BOT_TEST_REQUIRE_DB=1`). 필수면 주소가 없거나 psql·스키마 생성이 실패하는 것을 «건너뜀»이 아니라 실패로 본다.
    pub fn required() -> bool {
        std::env::var("BOT_TEST_REQUIRE_DB").is_ok_and(|value| value == "1")
    }

    /// 내 PID가 붙은 임시 스키마를 만든다. 시험 DB가 없으면 None(건너뜀)이지만, 필수 모드에서는 어떤 실패든 패닉으로 전파한다.
    pub fn create() -> Option<Self> {
        let required = Self::required();
        let fail = |why: &str| -> Option<Self> {
            if required {
                panic!("BOT_TEST_REQUIRE_DB=1인데 시험 DB를 쓸 수 없어요: {why}");
            }
            None
        };
        let Ok(base) = std::env::var("BOT_TEST_DATABASE_URL") else {
            return fail("BOT_TEST_DATABASE_URL이 없어요");
        };
        // 한 프로세스 안의 시험끼리 스키마를 나누지 않게 번호를 붙인다(정리는 이 PID의 것만)
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
                "스키마를 만들지 못했어요({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(error) => fail(&format!("psql을 실행하지 못했어요: {error}")),
        }
    }

    pub fn url(&self) -> String {
        format!("{}?options=-c%20search_path%3D{}", self.base, self.name)
    }

    /// 저장소에 게시된 활성 집합의 판본(없으면 빈 글)
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

    /// 임의의 조회 한 값. `{s}`는 이 스키마 이름으로 바뀐다.
    pub fn query(&self, sql: &str) -> String {
        let out = Command::new("psql")
            .args([&self.base, "-Atc", &sql.replace("{s}", &self.name)])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// 문장 하나를 돌려 명령 결과(`INSERT 0 1` 등)나 오류 글을 그대로 돌려준다. `{s}`는 이 스키마 이름으로 바뀐다. 오류도 글로 받아 시험이 보여 줄 수 있다.
    pub fn exec(&self, sql: &str) -> String {
        let out = Command::new("psql")
            .args([&self.base, "-Atc", &sql.replace("{s}", &self.name)])
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }

    /// 이 스키마에 만들어진 테이블 수(마이그레이션조차 하지 않았으면 0)
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

/// 전용 psql 연결의 트랜잭션이 활성 판본 행의 잠금을 보유한다.
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
        ).expect("잠금 요청");
        let output = lock.connection.stdout.take().expect("psql stdout");
        let mut reply = String::new();
        std::io::BufReader::new(output)
            .read_line(&mut reply)
            .expect("잠금 응답");
        assert_eq!(reply.trim(), "1", "활성 판본 행을 잠가야 한다");
        lock
    }
}

impl Drop for ActiveRowLock {
    fn drop(&mut self) {
        // EOF를 받은 psql이 연결을 닫으면 PostgreSQL이 미커밋 트랜잭션을 롤백한다.
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

/// 서버를 띄우고 소켓이 열릴 때까지 기다린다.
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
            .expect("serve 시작"),
    );
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(socket.exists(), "소켓이 열리지 않았어요");
    server
}

/// 역할을 지정해 서버를 띄우고 소켓이 열릴 때까지 기다린다(관리 화면도 함께 띄우는 시험용).
pub fn start_gateway_roles(envs: &[(&str, &str)], roles: &str, socket: &Path) -> Server {
    start_roles(envs, roles, &[], socket)
}

/// 역할(쉼표로 여럿 가능)과 추가 인자(`--activate` 등)를 지정해 서버를 띄우고 소켓이 열릴 때까지 기다린다.
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
            .expect("serve 시작"),
    );
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(socket.exists(), "소켓이 열리지 않았어요");
    server
}

/// 소켓을 기다리지 않고 역할 하나를 따로 프로세스로 띄운다(분리 배치 시험용).
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
            .expect("serve 시작"),
    )
}

/// PATH에서 실행 파일을 찾는다.
pub fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// 도구 샌드박스(`srt`)와 `python3`가 있는가. 없으면 건너뛰지만, 필수 모드(`BOT_SRT_CANARY=required` — 공개 관문 `--strict`)에서는 실패한다.
/// 건너뛰는 시험은 `skipped:`로 시작하는 줄을 남긴다(관문이 «건너뜀 0»을 확인한다).
pub fn have_srt_and_python() -> bool {
    let ok = which("srt").is_some() && which("python3").is_some();
    if !ok {
        assert!(
            std::env::var("BOT_SRT_CANARY").as_deref() != Ok("required"),
            "BOT_SRT_CANARY=required: srt와 python3가 필요하다"
        );
        eprintln!("skipped: srt 또는 python3가 없어 건너뜀");
    }
    ok
}

/// `srt`가 있는가(필수 모드에서는 없으면 실패한다). `python3`는 보지 않는다.
pub fn have_srt() -> bool {
    let ok = which("srt").is_some();
    if !ok {
        assert!(
            std::env::var("BOT_SRT_CANARY").as_deref() != Ok("required"),
            "BOT_SRT_CANARY=required: srt가 필요하다"
        );
        eprintln!("skipped: srt가 없어 건너뜀");
    }
    ok
}

/// 조건이 될 때까지 기다린다(소켓은 활성 판본을 확정하기 전에 먼저 열리므로 소켓이 있다고 게시가 끝난 것은 아니다).
pub fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..100 {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what}");
}

/// 환경 변수 쌍(소유 문자열). `bot`·`start_gateway`가 받는 빌린 쌍으로 바꿔 쓴다.
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

/// 원시 HTTP/1.1 GET. 상태 코드와 본문을 돌려준다.
pub fn http_get(addr: &str, path: &str, token: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("관리 화면에 접속");
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

/// 이 프로세스를 실행한 OS 사용자 번호
pub fn my_uid() -> u32 {
    let out = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .unwrap()
        .stdout;
    String::from_utf8_lossy(&out).trim().parse().unwrap()
}

/// `bin/server`(실행 스크립트)가 있는 도구 서버 패키지 폴더를 만든다.
pub fn package(dir: &Path, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let root = dir.join("pkg");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let server = root.join("bin/server");
    std::fs::write(&server, script).unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    root
}

/// 호스트 설정 JSON: CLI 계정 `owner` → `uid`, 데모 서버 별칭의 설치 명세, 그리고 덧붙일 최상위 절(`,"admin": …` 꼴).
pub fn host_json(uid: u32, server_root: &Path, extra_gateway: &str, extra_top: &str) -> String {
    format!(
        r#"{{"version": 1,
            "servers": {{"demo-agenda": {{"root": "{root}", "command": ["bin/server", "--stdio"]}}}},
            "models": {{"personal": ["assistant"]}},
            "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": {uid}}}}}}}{extra_gateway}}}{extra_top}}}"#,
        root = server_root.display()
    )
}

/// 원시 HTTP/1.1 POST(JSON). 상태 코드와 본문을 돌려준다.
pub fn http_post(addr: &str, path: &str, token: &str, csrf: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("관리 화면에 접속");
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

/// 예시 프로필에 HTTP 입구 `inbox`(주체 `http:inbox/svc`)와 `POST /notify` 트리거 하나를 붙인 사본을 `to`에 만든다. 호스트 설정에 `inbox` 입구를 달면
/// 그 키 이름의 자격이 프로필이 선언한 비밀 이름이 된다.
pub fn with_inbox(to: &Path) {
    copy_dir(&examples(), to);
    let yaml_path = to.join("personal/profile.yaml");
    let yaml = std::fs::read_to_string(&yaml_path).unwrap();
    let desk = "  - {id: desk, adapter: cli, account: owner, agent: main}";
    let owner = "  - {identity: \"local:owner\", roles: [owner]}";
    assert!(
        yaml.contains(desk) && yaml.contains(owner),
        "예시 프로필이 바뀌었어요"
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

/// 지금 비어 있는 로컬 포트 하나(바로 닫아 돌려준다. 곧 이어 쓰는 시험용이라 잠깐 겹칠 수 있다).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
