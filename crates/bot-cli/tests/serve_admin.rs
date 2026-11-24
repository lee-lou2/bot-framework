//! 관리 화면과 관측 시험(실제 프로세스·팀 시험 DB): 운영자 비밀은 터미널 또는 0600 파일로만 전달되고, 구판 프로세스의 관리 응답은
//! 내용 없이 503으로 닫히며, gateway만 띄워도 설정한 수집기로 span이 나가되 요청이 경로에 실은 식별자·비밀은 나가지 않고,
//! 수집을 거두면 옛 프로세스는 더 내보내지 않는다.
#[path = "serve_admin/observe.rs"]
mod observe;
mod process_support;
use process_support::*;
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    time::Duration,
};

#[test]
fn the_admin_role_opens_an_authenticated_screen_and_never_puts_the_secret_on_a_pipe() {
    use std::os::unix::fs::PermissionsExt;
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
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
        .expect("serve 시작");
    let stdout = child.stdout.take().unwrap();
    let mut server = Server(child);
    // 표준출력은 모두 모은다(파이프에 비밀이 나가는지 보려고). 주소 줄이 보이면 알리고, 읽기는 끝까지 이어 서버가 막히지 않게 한다.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains("관리 화면: http://") {
                    let _ = tx.send(line.clone());
                }
                seen.lock().unwrap().push(line);
            }
        })
    };
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("관리 화면 주소 줄이 없어요");
    let addr = line
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split("/ui").next())
        .expect("주소")
        .to_owned();
    // 운영자 비밀은 지정한 0600 파일에만 있다(무작위 32바이트, 16진 64자)
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
        "파이프 표준출력에 비밀이 나갔어요: {:?}",
        seen.lock().unwrap()
    );

    // 비밀 없이는 열리지 않고, 비밀이 있으면 운영자 신원(이 OS 사용자의 CLI 계정)으로 열린다
    assert_eq!(http_get(&addr, "/v1/admin/me", None).0, 401);
    assert_eq!(http_get(&addr, "/v1/admin/me", Some("wrong")).0, 401);
    let (status, body) = http_get(&addr, "/v1/admin/me", Some(&token));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("local:owner"), "{body}");
    assert_eq!(http_get(&addr, "/v1/admin/status", Some(&token)).0, 200);

    // 정상 상태에서는 지도에 프로필이 보이고 승인 목록도 열린다
    let (_, map) = http_get(&addr, "/v1/admin/map", Some(&token));
    assert!(map.contains("personal"), "{map}");
    assert_eq!(http_get(&addr, "/v1/admin/approvals", Some(&token)).0, 200);
    // 다른 프로세스가 활성 집합을 교체했다(DB의 활성 판본만 바꾼다). 이 프로세스는 구판이라 내용 없이 503으로 닫는다:
    // 옛 입구 매핑으로 지도·카드·신원을 내주지 않는다.
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
            "{path}: 이전 내용이 나갔어요: {body}"
        );
    }
    // 인증 실패는 구판 확인 이전에 막히고(401), 정적 화면은 열려 «사용할 수 없음»을 보일 수 있다
    assert_eq!(http_get(&addr, "/v1/admin/me", None).0, 401);
    assert_eq!(http_get(&addr, "/ui", Some(&token)).0, 200);

    // 정상 종료(SIGTERM)하면 비밀 파일도 지운다
    let pid = server.0.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    wait_for("종료하지 않았어요", || {
        server.0.try_wait().ok().flatten().is_some()
    });
    wait_for("비밀 파일이 남았어요", || !token_path.exists());
    reader.join().unwrap();
    assert!(seen.lock().unwrap().iter().all(|l| !l.contains(&token)));
    // 운영자 비밀은 저장소에도 남지 않는다
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
        "감사에 비밀이 남았어요: {count}"
    );
}

#[test]
fn an_admin_start_without_a_terminal_or_a_token_file_stops_before_publishing_anything() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
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
    // `bot()`은 표준출력을 파이프로 받으므로 터미널이 아니다
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
        "비밀을 전달할 수 없는 시작은 아무것도 게시하지 않아요"
    );
}
