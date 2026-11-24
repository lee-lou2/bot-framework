//! `bot serve`의 시작 순서 시험(실제 프로세스·팀 시험 DB): 시작이 실패하면 운영 판본이 바뀌지 않고, 관리 화면은 인증된 운영자만
//! 열고, 입구 매핑이 바뀌면 활성화 사건이 되어 옛 게이트웨이를 닫는다.
mod process_support;
use process_support::*;
use std::path::{Path, PathBuf};

#[test]
fn a_start_that_fails_before_opening_never_changes_the_operating_revision() {
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
    let url = schema.url();
    // 운영 중인 게이트웨이 A(원래 판본)
    let server = start_gateway(&refs(&env_for(&profiles, &url, &socket, &[])), &[], &socket);
    wait_for(
        "첫 시작이 활성 판본을 게시하지 않았어요",
        || !schema.active().is_empty(),
    );
    let operating = schema.active();

    // 새 판본 B로 --activate: 소켓을 A가 쓰고 있어 시작이 실패하고, 운영 판본은 그대로여야 한다
    let changed = dir.path().join("profiles");
    copy_dir(&profiles, &changed);
    std::fs::write(changed.join("personal/agents/main/AGENT.md"), "다른 지침").unwrap();
    let env = env_for(&changed, &url, &socket, &[]);
    let busy = bot(&refs(&env), &["serve", "--role", "gateway", "--activate"]);
    assert_eq!(busy.status.code(), Some(1), "{}", text(&busy));
    assert!(text(&busy).contains("이미 실행 중인"), "{}", text(&busy));
    assert_eq!(
        schema.active(),
        operating,
        "실패한 시작이 활성 판본을 바꿨어요"
    );

    // 입구 자격이 없어도 같다: HTTP 입구를 붙인 C는 키가 없어 시작하지 못하고 판본은 그대로다
    let keyless = dir.path().join("keyless");
    copy_dir(&changed, &keyless);
    let profile_yaml = keyless.join("personal/profile.yaml");
    let yaml = std::fs::read_to_string(&profile_yaml)
        .unwrap()
        .replace(
            "  - {id: desk, adapter: cli, account: owner, agent: main}",
            "  - {id: desk, adapter: cli, account: owner, agent: main}\n  - {id: inbox, adapter: http, endpoint: inbox, agent: main}",
        )
        .replace(
            "  - {identity: \"local:owner\", roles: [owner]}",
            "  - {identity: \"local:owner\", roles: [owner]}\n  - {identity: \"http:inbox/svc\", roles: [reader]}",
        );
    std::fs::write(&profile_yaml, yaml).unwrap();
    let host = dir.path().join("host.json");
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            r#", "http": {"listen": "127.0.0.1:0",
                "endpoints": {"inbox": {"issuer": "i", "audience": "a", "key": "inbox-key"}}}"#,
            "",
        ),
    )
    .unwrap();
    let env = env_for(
        &keyless,
        &url,
        &socket,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    let missing = bot(&refs(&env), &["serve", "--role", "gateway", "--activate"]);
    assert!(!missing.status.success(), "{}", text(&missing));
    let message = text(&missing);
    assert!(
        message.contains("inbox-key") && message.contains("BOT_SECRET__personal__inbox_key"),
        "{message}"
    );
    assert_eq!(
        schema.active(),
        operating,
        "자격이 없어 실패한 시작이 판본을 바꿨어요"
    );

    // A를 내리면 같은 B가 --activate로 시작하고 그제야 활성 판본이 바뀐다
    drop(server);
    let env = env_for(&changed, &url, &socket, &[]);
    let server = start_gateway(&refs(&env), &["--activate"], &socket);
    wait_for(
        "정상 시작이 새 판본을 게시하지 않았어요",
        || schema.active() != operating,
    );
    drop(server);
}

/// 데모 서버 설치와 CLI 계정 `owner`(→ `uid`)만 있는 호스트 설정 파일을 쓰고 경로를 돌려준다.
fn write_host(dir: &Path, name: &str, uid: u32) -> PathBuf {
    let pkg = package(dir, "#!/bin/sh\nexit 0\n");
    let path = dir.join(name);
    std::fs::write(&path, host_json(uid, &pkg, "", "")).unwrap();
    path
}

#[test]
fn a_changed_identity_mapping_is_an_activation_event_that_closes_the_old_gateway() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let profiles = examples();
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let (socket_a, socket_b) = (
        dir.path().join("a").join("g.sock"),
        dir.path().join("b").join("g.sock"),
    );
    let url = schema.url();
    // 프로필은 같고 CLI 계정 owner가 가리키는 OS 사용자 번호만 다른 호스트 설정 둘
    let host_a = write_host(dir.path(), "host-a.json", my_uid());
    let host_b = write_host(dir.path(), "host-b.json", my_uid() + 1);
    let env_a = env_for(
        &profiles,
        &url,
        &socket_a,
        &[("BOT_HOST_CONFIG", host_a.to_str().unwrap())],
    );
    let env_b = env_for(
        &profiles,
        &url,
        &socket_b,
        &[("BOT_HOST_CONFIG", host_b.to_str().unwrap())],
    );
    let a = start_gateway(&refs(&env_a), &[], &socket_a);
    wait_for(
        "첫 시작이 활성 판본을 게시하지 않았어요",
        || !schema.active().is_empty(),
    );
    let operating = schema.active();
    // 같은 호스트 설정으로 하나 더 띄우면 쓰기 없이 합류한다(같은 매핑). 접수를 받을 때까지 기다려 확정 단계가 지난 뒤를 본다.
    let publication_writes = || {
        schema.query(
            "select count(*) from {s}.bot_audit where action in ('active.replace', 'memory.seed')",
        )
    };
    let (audit, memories) = (publication_writes(), schema.count("bot_memories"));
    let socket_c = dir.path().join("c").join("g.sock");
    let env_c = env_for(
        &profiles,
        &url,
        &socket_c,
        &[("BOT_HOST_CONFIG", host_a.to_str().unwrap())],
    );
    let joined = start_gateway(&refs(&env_c), &[], &socket_c);
    let hello = bot(
        &refs(&env_c),
        &["chat", "--no-wait", "--request-id", "join-1", "안녕"],
    );
    assert!(hello.status.success(), "{}", text(&hello));
    assert_eq!(schema.active(), operating);
    assert_eq!(
        (publication_writes(), schema.count("bot_memories")),
        (audit.clone(), memories),
        "합류는 게시 감사·기억을 쓰지 않아요"
    );
    assert_ne!(audit, "0", "첫 게시는 감사를 남겼어요");
    drop(joined);

    // 매핑만 다른 게이트웨이는 명시 활성화 없이는 같은 판본에 합류하지 못한다
    let refused = bot(&refs(&env_b), &["serve", "--role", "gateway"]);
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(text(&refused).contains("--activate"), "{}", text(&refused));
    assert_eq!(schema.active(), operating);
    // 명시 활성화하면 새 매핑이 활성 판본이 되고
    let b = start_gateway(&refs(&env_b), &["--activate"], &socket_b);
    wait_for(
        "매핑 변경이 활성 판본을 바꾸지 않았어요",
        || schema.active() != operating,
    );
    // 옛 매핑으로 뜬 게이트웨이는 구판으로 닫힌다: 접수하지 않고 일시 실패(종료 코드 3)로 답한다
    let runs = schema.count("bot_runs");
    let stale = bot(
        &refs(&env_a),
        &["chat", "--no-wait", "--request-id", "stale-1", "안녕"],
    );
    assert_eq!(stale.status.code(), Some(3), "{}", text(&stale));
    assert_eq!(schema.count("bot_runs"), runs, "구판은 접수하지 않아요");
    drop((a, b));
}

#[test]
fn a_changed_model_allowance_or_egress_cap_is_an_activation_event() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let profiles = examples();
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let write = |name: &str, extra: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, host_json(my_uid(), &pkg, "", extra)).unwrap();
        path
    };
    let base = write("base.json", "");
    let wider = write(
        "wider.json",
        r#", "egress": {"personal": ["calendar.example:443"]}"#,
    );
    let url = schema.url();
    let socket = |name: &str| dir.path().join(name).join("g.sock");
    let env = |host: &Path, name: &str| {
        env_for(
            &profiles,
            &url,
            &socket(name),
            &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
        )
    };
    let server = start_gateway(&refs(&env(&base, "a")), &[], &socket("a"));
    wait_for(
        "첫 시작이 활성 판본을 게시하지 않았어요",
        || !schema.active().is_empty(),
    );
    let operating = schema.active();
    // egress 상한만 달라진 호스트 설정은 명시 활성화 없이는 합류하지 못한다(구판 워커가 철회된 목적지를 쓰지 못하게)
    let refused = bot(&refs(&env(&wider, "b")), &["serve", "--role", "gateway"]);
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(text(&refused).contains("--activate"), "{}", text(&refused));
    assert_eq!(schema.active(), operating);
    // 검증 화면의 활성화 판본도 달라진다
    let shown = |host: &Path| {
        let out = bot(&refs(&env(host, "v")), &["validate"]);
        assert!(out.status.success(), "{}", text(&out));
        text(&out)
            .lines()
            .find(|line| line.starts_with("활성화 판본 "))
            .map(str::to_owned)
            .unwrap()
    };
    assert_ne!(shown(&base), shown(&wider));
    drop(server);
}

/// 프로필에 HTTP 입구(`inbox`)를 더한 복사본
fn with_http_entrance(from: &Path, to: &Path) {
    copy_dir(from, to);
    let yaml_path = to.join("personal/profile.yaml");
    let yaml = std::fs::read_to_string(&yaml_path)
        .unwrap()
        .replace(
            "  - {id: desk, adapter: cli, account: owner, agent: main}",
            "  - {id: desk, adapter: cli, account: owner, agent: main}\n  - {id: inbox, adapter: http, endpoint: inbox, agent: main}",
        )
        .replace(
            "  - {identity: \"local:owner\", roles: [owner]}",
            "  - {identity: \"local:owner\", roles: [owner]}\n  - {identity: \"http:inbox/svc\", roles: [reader]}",
        );
    std::fs::write(yaml_path, yaml).unwrap();
}

#[test]
fn an_http_key_shorter_than_32_bytes_stops_the_start_and_the_validation() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let profiles = dir.path().join("profiles");
    with_http_entrance(&examples(), &profiles);
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            r#", "http": {"listen": "127.0.0.1:0",
                "endpoints": {"inbox": {"issuer": "i", "audience": "a", "key": "inbox-key"}}}"#,
            "",
        ),
    )
    .unwrap();
    let url = schema.url();
    let socket = dir.path().join("run").join("g.sock");
    let short = "k".repeat(31);
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_SECRET__personal__inbox_key", short.as_str()),
        ],
    );
    let validate = bot(&refs(&env), &["validate"]);
    assert!(!validate.status.success(), "{}", text(&validate));
    assert!(
        text(&validate).contains("너무 짧아요"),
        "{}",
        text(&validate)
    );
    assert!(
        !text(&validate).contains("통과"),
        "문제가 있는데 통과를 알렸어요: {}",
        text(&validate)
    );
    let serve = bot(&refs(&env), &["serve", "--role", "gateway", "--activate"]);
    assert!(!serve.status.success(), "{}", text(&serve));
    assert!(text(&serve).contains("너무 짧아요"), "{}", text(&serve));
    assert_eq!(
        schema.active(),
        "",
        "실패한 시작이 아무것도 게시하지 않아요"
    );
    // 하한(32바이트)이면 점검을 통과한다
    let exact = "k".repeat(32);
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_SECRET__personal__inbox_key", exact.as_str()),
        ],
    );
    let validate = bot(&refs(&env), &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
}

#[test]
fn a_memory_seed_over_the_store_limit_stops_the_validation_and_the_start_before_any_write() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let profiles = dir.path().join("profiles");
    copy_dir(&examples(), &profiles);
    let seed = profiles.join("personal/agents/main/memory/routine.md");
    let body = "가".repeat(2731); // 8,193바이트
    std::fs::write(&seed, format!("---\nowner: \"local:owner\"\n---\n{body}")).unwrap();
    let url = schema.url();
    let socket = dir.path().join("run").join("g.sock");
    let env = env_for(&profiles, &url, &socket, &[]);
    let validate = bot(&refs(&env), &["validate"]);
    assert!(!validate.status.success(), "{}", text(&validate));
    let message = text(&validate);
    assert!(
        message.contains("너무 길어요") && message.contains("memory/routine.md"),
        "{message}"
    );
    assert!(
        !message.contains("통과"),
        "문제가 있는데 통과를 알렸어요: {message}"
    );
    let serve = bot(&refs(&env), &["serve", "--role", "gateway", "--activate"]);
    assert!(!serve.status.success(), "{}", text(&serve));
    assert!(text(&serve).contains("너무 길어요"), "{}", text(&serve));
    // 저장소에 접속하기 전에 멈춘다: 마이그레이션조차 하지 않아 스키마에 아무것도 없다
    assert_eq!(schema.tables(), "0", "아무것도 쓰지 않아요");
}
