//! 관리 비밀 파일의 수명 시험(실제 프로세스·팀 시험 DB): 시작이 실패하거나 강제 종료돼도 원래 파일이 지워지거나 바뀐 채 남지 않고,
//! 실행 중인 관리 화면의 파일은 다른 시작이 건드리지 못하며, 비밀을 발행하지 못하면 활성 판본이 바뀌기 전에 멈춘다.
mod process_support;
use process_support::*;
use std::{
    path::Path,
    process::{Command, Stdio},
};

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[test]
fn a_failed_admin_start_never_deletes_the_secret_files_of_a_running_admin_or_an_existing_one() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let host = dir.path().join("host.json");
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            "",
            &format!(r#", "admin": {{"bind": "{addr}"}}"#),
        ),
    )
    .unwrap();
    let url = schema.url();
    let profiles = examples();
    let envs = |socket: &str, token_file: &Path| {
        env_for(
            &profiles,
            &url,
            &dir.path().join(socket).join("g.sock"),
            &[
                ("BOT_HOST_CONFIG", host.to_str().unwrap()),
                ("BOT_ADMIN_TOKEN_FILE", token_file.to_str().unwrap()),
            ],
        )
    };
    // 실행 중인 관리 화면 A: 자기 비밀 파일을 발행한다
    let token_a = dir.path().join("a.token");
    let env_a = envs("a", &token_a);
    let server = start_gateway_roles(
        &refs(&env_a),
        "gateway,admin",
        &dir.path().join("a").join("g.sock"),
    );
    wait_for("A가 비밀 파일을 발행하지 않았어요", || {
        token_a.exists()
    });
    let secret = std::fs::read_to_string(&token_a).unwrap();
    let ok = |token: &str| http_get(&addr, "/v1/admin/me", Some(token.trim())).0 == 200;
    assert!(ok(&secret));

    // 같은 비밀 파일 경로로 하나 더 띄우면 독점하지 못하고 A의 파일은 그대로다
    let env_b = envs("b", &token_a);
    let second = bot(&refs(&env_b), &["serve", "--role", "admin"]);
    assert!(!second.status.success(), "{}", text(&second));
    assert!(text(&second).contains("다른 프로세스"), "{}", text(&second));
    assert_eq!(std::fs::read_to_string(&token_a).unwrap(), secret);
    assert!(ok(&secret), "A는 계속 같은 비밀로 열려 있다");

    // 이미 쓰는 포트로 시작하면 실패하는데, 그 경로에 원래 있던 0600 파일은 그대로 남는다
    let token_c = dir.path().join("c.token");
    std::fs::write(&token_c, "원래 있던 비밀\n").unwrap();
    let env_c = envs("c", &token_c);
    let busy = bot(&refs(&env_c), &["serve", "--role", "admin"]);
    assert!(!busy.status.success(), "{}", text(&busy));
    assert_eq!(
        std::fs::read_to_string(&token_c).unwrap(),
        "원래 있던 비밀\n",
        "실패한 시작이 원래 파일을 지우거나 바꿨어요"
    );
    assert_eq!(std::fs::read_to_string(&token_a).unwrap(), secret);
    assert!(ok(&secret));
    drop(server);
}

#[test]
fn a_failed_secret_publication_stops_the_start_before_the_active_revision_changes() {
    use std::os::unix::fs::PermissionsExt;
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let url = schema.url();
    let profiles = examples();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(my_uid(), &pkg, "", r#", "admin": {"bind": "127.0.0.1:0"}"#),
    )
    .unwrap();
    // 운영 중인 판본(원래 프로필)
    let socket = dir.path().join("a").join("g.sock");
    let env_a = env_for(
        &profiles,
        &url,
        &socket,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    let server = start_gateway(&refs(&env_a), &[], &socket);
    wait_for(
        "첫 시작이 활성 판본을 게시하지 않았어요",
        || !schema.active().is_empty(),
    );
    let operating = schema.active();

    // 새 판본으로 관리 화면을 --activate로 시작하는데 비밀 파일을 발행할 수 없다(폴더에 더 쓸 수 없음):
    // 독점(잠금 파일은 미리 있음)은 되지만 발행이 실패한다. 판본은 바뀌지 않아야 하고 원래 파일은 그대로여야 한다.
    let changed = dir.path().join("profiles");
    copy_dir(&profiles, &changed);
    std::fs::write(changed.join("personal/agents/main/AGENT.md"), "다른 지침").unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let token = locked.join("admin.token");
    std::fs::write(&token, "원래 있던 비밀\n").unwrap();
    std::fs::write(locked.join("admin.token.lock"), "").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let env_b = env_for(
        &changed,
        &url,
        &dir.path().join("b").join("g.sock"),
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_ADMIN_TOKEN_FILE", token.to_str().unwrap()),
        ],
    );
    let failed = bot(&refs(&env_b), &["serve", "--role", "admin", "--activate"]);
    assert!(!failed.status.success(), "{}", text(&failed));
    assert!(text(&failed).contains("admin.token"), "{}", text(&failed));
    assert_eq!(
        schema.active(),
        operating,
        "비밀 발행에 실패한 시작이 활성 판본을 바꿨어요"
    );
    assert_eq!(std::fs::read_to_string(&token).unwrap(), "원래 있던 비밀\n");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    drop(server);
}

#[test]
fn a_forced_kill_between_the_secret_and_the_commit_is_repaired_by_the_next_start() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let url = schema.url();
    let profiles = examples();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let write_host = |name: &str, extra: &str| {
        let path = dir.path().join(name);
        std::fs::write(
            &path,
            host_json(
                my_uid(),
                &pkg,
                "",
                &format!(r#", "admin": {{"bind": "127.0.0.1:0"}}{extra}"#),
            ),
        )
        .unwrap();
        path
    };
    // 운영 중인 판본
    let base = write_host("base.json", "");
    let socket = dir.path().join("a").join("g.sock");
    let env_a = env_for(
        &profiles,
        &url,
        &socket,
        &[("BOT_HOST_CONFIG", base.to_str().unwrap())],
    );
    let server = start_gateway(&refs(&env_a), &[], &socket);
    wait_for(
        "첫 시작이 활성 판본을 게시하지 않았어요",
        || !schema.active().is_empty(),
    );
    let operating = schema.active();

    // 저장소의 활성 판본 행을 다른 세션이 잡고 있게 해서, 새 관리 프로세스가 비밀을 발행한 뒤 판본 확정에서 멈추게 한다
    let holder = schema.hold_active_row();

    // 새 판본(egress 변경)을 --activate로 시작: 비밀 파일을 발행하고 확정에서 막힌다
    let wider = write_host(
        "wider.json",
        r#", "egress": {"personal": ["calendar.example:443"]}"#,
    );
    let token = dir.path().join("admin.token");
    std::fs::write(&token, "원래 있던 비밀\n").unwrap();
    let env_b = env_for(
        &profiles,
        &url,
        &dir.path().join("b").join("g.sock"),
        &[
            ("BOT_HOST_CONFIG", wider.to_str().unwrap()),
            ("BOT_ADMIN_TOKEN_FILE", token.to_str().unwrap()),
        ],
    );
    let mut blocked = Server(
        Command::new(BOT)
            .args(["serve", "--role", "admin", "--activate"])
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
            .envs(refs(&env_b))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("admin 시작"),
    );
    wait_for(
        "새 프로세스가 비밀 파일을 발행하지 않았어요",
        || std::fs::read_to_string(&token).is_ok_and(|text| text != "원래 있던 비밀\n"),
    );
    let dead_token = std::fs::read_to_string(&token).unwrap();
    assert_eq!(schema.active(), operating, "확정 전이라 판본은 옛것이다");

    // 확정 전에 강제 종료한다(되돌릴 기회가 없다)
    blocked.0.kill().unwrap();
    blocked.0.wait().unwrap();
    drop(holder); // 이 시험이 연 psql 연결을 닫고 행 잠금을 놓는다.
    assert_eq!(
        std::fs::read_to_string(&token).unwrap(),
        dead_token,
        "죽은 프로세스의 비밀이 남았다"
    );
    let leftovers = || -> Vec<String> {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.contains(".bak-") || name.contains(".tmp-") || name.ends_with(".journal")
            })
            .collect()
    };
    assert_eq!(
        leftovers().len(),
        2,
        "원래 파일이 보관본에만 있고 복구 기록이 남았다: {:?}",
        leftovers()
    );
    assert_eq!(schema.active(), operating);

    // 다음 기동이 경로 잠금을 잡으면 복구 기록이 가리키는 파일로 원래 파일을 되돌린다. 이번 시작은 일부러 실패시킨다(쓰는 주소를 선점)
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = dir.path().join("busy.json");
    // 선점한 주소로 관리 화면을 열도록 설정을 다시 쓴다
    std::fs::write(
        &busy,
        host_json(
            my_uid(),
            &pkg,
            "",
            &format!(
                r#", "admin": {{"bind": "{}"}}, "egress": {{"personal": ["calendar.example:443"]}}"#,
                squatter.local_addr().unwrap()
            ),
        ),
    )
    .unwrap();
    let env_c = env_for(
        &profiles,
        &url,
        &dir.path().join("c").join("g.sock"),
        &[
            ("BOT_HOST_CONFIG", busy.to_str().unwrap()),
            ("BOT_ADMIN_TOKEN_FILE", token.to_str().unwrap()),
        ],
    );
    let failed = bot(&refs(&env_c), &["serve", "--role", "admin", "--activate"]);
    assert!(!failed.status.success(), "{}", text(&failed));
    assert_eq!(
        std::fs::read_to_string(&token).unwrap(),
        "원래 있던 비밀\n",
        "죽은 비밀이 아니라 원래 파일로 복구됐어야 해요"
    );
    assert_eq!(
        leftovers(),
        Vec::<String>::new(),
        "보관본과 복구 기록이 남지 않았어요"
    );
    assert_eq!(
        schema.active(),
        operating,
        "실패한 시작은 판본을 바꾸지 않아요"
    );
    drop((squatter, server));
}
