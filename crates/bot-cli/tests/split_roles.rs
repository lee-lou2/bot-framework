//! 분리 배치 수락 시험(`starts_split_roles`): gateway·scheduler·worker·admin을 **각각 다른 프로세스**로 띄우고, 같은 저장소를 통해
//! CLI 대화가 데모 모델과 srt 샌드박스 안의 합성 일정 서버로 끝까지 가는지, 위험한 삭제는 본인 승인 뒤에만 실행되는지 본다.
//! 팀 시험 DB·`srt`·`python3`가 있어야 하고 없으면 건너뛴다.
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

/// 합성 일정 서버(예제)와 데모 모델을 쓰는 호스트 설정
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
    // 역할마다 따로 프로세스: gateway가 먼저 활성 판본을 게시하고 나머지는 합류한다
    let gateway = start_gateway_roles(&refs, "gateway", &socket);
    wait_for("활성 판본이 게시되지 않았어요", || {
        !schema.active().is_empty()
    });
    let worker = spawn_role(&refs, "worker");
    let scheduler = spawn_role(&refs, "scheduler");
    // 관리 화면은 표준출력의 주소 줄에서 주소를 읽는다(비밀은 파일에서 읽는다)
    let mut child = Command::new(BOT)
        .args(["serve", "--role", "admin"])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(refs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("admin 시작");
    let stdout = child.stdout.take().unwrap();
    let admin = Server(child);
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
    // 읽기 요청: 데모 모델이 srt 안의 합성 일정 서버에서 일정을 읽어 답한다
    let chat = bot(
        &env,
        &["chat", "--request-id", "split-list", "오늘 일정 알려줘"],
    );
    assert!(chat.status.success(), "{}", text(&chat));
    assert!(text(&chat).contains("산책"), "{}", text(&chat));
    assert_eq!(
        split
            .schema
            .query("select count(*) from {s}.bot_runs where state = 'succeeded'"),
        "1"
    );
    // 이 분리 배치에서 예약 정의가 맞춰졌다(scheduler)
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
            "산책 일정을 삭제해줘",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    // 승인 카드가 열릴 때까지 기다린다(관리 화면의 승인 목록)
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
        "승인 카드가 열리지 않았어요: 목록 {last} / 실행 {}",
        split
            .schema
            .query("select string_agg(id || ':' || state || ':' || coalesce(last_error, ''), ' | ') from {s}.bot_runs")
    );
    // 승인 전에는 삭제가 실행되지 않았다
    let early = bot(
        &env,
        &["chat", "--attach", "--no-wait", "--request-id", "split-del"],
    );
    assert!(early.status.success(), "{}", text(&early));
    assert!(
        !text(&early).contains("sim-"),
        "승인 전에 삭제가 실행됐어요: {}",
        text(&early)
    );
    // 본인(운영자)이 승인한다: 화면이 본 인자 해시와 서버가 준 CSRF 토큰이 필요하다
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
    // 승인 뒤 실행되어 가상 삭제 영수증으로 끝난다
    let done = bot(&env, &["chat", "--attach", "--request-id", "split-del"]);
    assert!(done.status.success(), "{}", text(&done));
    assert!(
        text(&done).contains("시뮬레이션했어요") && text(&done).contains("가상 영수증 sim-"),
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

/// 전 역할을 한 프로세스(`--role all`)로 띄운다. 관리 화면 비밀은 파일로 받는다(표준출력은 터미널이 아니다).
#[test]
#[ignore = "예약(cron) 한 분을 기다려 최대 1분 걸려요 — cargo test -- --ignored"]
fn runs_personal_demo() {
    let Some(schema) = Schema::create() else {
        return;
    };
    if !have_srt_and_python() {
        assert!(
            !Schema::required(),
            "BOT_TEST_REQUIRE_DB=1에서 runs_personal_demo는 srt와 python3가 필요해요"
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
    // 대화: 일정 조회
    let chat = bot(
        &env,
        &["chat", "--request-id", "demo-list", "오늘 일정 알려줘"],
    );
    assert!(chat.status.success(), "{}", text(&chat));
    assert!(text(&chat).contains("산책"), "{}", text(&chat));
    // 예약: morning은 매분 agenda.list를 직접 실행한다(모델 호출 0회). 다음 분에 예약 run이 끝난다.
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
        "예약 run이 끝나지 않았어요: {seen}"
    );
    // 예약 run은 모델 없이 도구를 직접 불렀다: 대상이 도구이고 에이전트 대화 기록이 없다
    assert_ne!(
        schema.query(
            "select count(*) from {s}.bot_runs where trigger = 'morning' and target like 'tool%'"
        ),
        "0"
    );
}
