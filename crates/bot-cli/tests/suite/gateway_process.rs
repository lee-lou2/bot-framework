//! 실제 `bot` 프로세스 시험: `serve --role gateway`가 접수를 받고(`chat`), 같은 요청 ID 재시도는 같은 접수이고,
//! 같은 ID에 다른 글은 충돌이며, 시작 점검이 어긋나면 아무것도 열지 않는다.
//! 팀 시험 DB(`BOT_TEST_DATABASE_URL`, 예: postgres://<사용자>@localhost:<포트>/bot_test)와 `psql`이 있어야 하고,
//! 없으면 DB가 필요한 시험은 건너뛴다. 시험 스키마는 이 프로세스의 PID가 붙은 것만 만들고 지운다.
use crate::process_support::*;
use std::{
    process::{Command, Stdio},
    time::Duration,
};

#[test]
fn validate_and_map_work_on_the_example_without_any_service() {
    let profiles = examples();
    let envs = [("BOT_PROFILES", profiles.to_str().unwrap())];
    let validate = bot(&envs, &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
    assert!(text(&validate).contains("검증 통과: 프로필 1개"));
    // 내장 데모 호스트는 합성 일정 서버를 연결하고 설치 폴더에서 판본을 계산한다
    assert!(
        text(&validate).contains("서버 demo-agenda: 판본 "),
        "{}",
        text(&validate)
    );
    let map = bot(&envs, &["map", "--json"]);
    assert!(map.status.success(), "{}", text(&map));
    let json: serde_json::Value = serde_json::from_slice(&map.stdout).unwrap();
    assert!(
        json["graph"]["nodes"]
            .as_array()
            .is_some_and(|n| !n.is_empty())
    );
    // 없는 프로필·잘못된 설정은 실패로 끝난다
    assert!(!bot(&envs, &["map", "--profile", "nobody"]).status.success());
    assert!(
        !bot(&[], &["validate"]).status.success(),
        "BOT_PROFILES 없이는 점검할 수 없다"
    );
    let bad_secrets = bot(
        &[
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_SECRETS", "vault"),
        ],
        &["validate"],
    );
    assert_eq!(bad_secrets.status.code(), Some(2));
}

#[test]
fn a_broker_alone_is_refused_with_an_explanation_and_starts_nothing() {
    let profiles = examples();
    let output = bot(
        &[("BOT_PROFILES", profiles.to_str().unwrap())],
        &["serve", "--role", "broker"],
    );
    assert!(!output.status.success());
    let message = text(&output);
    // 브로커(도구 실행·승인)는 worker에 포함돼 있다 — 따로 요청하면 이유를 밝히고 아무것도 시작하지 않는다
    assert!(message.contains("worker에 포함돼 있어요"), "{message}");
    assert!(message.contains("아무것도 시작하지 않아요"), "{message}");
    // `all`은 모든 역할이 조립돼 있어 이 단계를 지나 저장소 주소를 요구한다
    let all = bot(
        &[("BOT_PROFILES", profiles.to_str().unwrap())],
        &["serve", "--role", "all"],
    );
    assert!(!all.status.success());
    assert!(text(&all).contains("BOT_DATABASE_URL"), "{}", text(&all));
}

#[test]
fn chat_without_a_server_says_how_to_start_one() {
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("none.sock");
    let output = bot(
        &[("BOT_SOCKET", socket.to_str().unwrap())],
        &["chat", "안녕"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output).contains("bot serve가 실행 중인지"));
}

/// 내 PID가 붙은 임시 스키마. 끝나면 지운다.
#[test]
fn a_gateway_process_accepts_chat_idempotently_and_reports_conflicts() {
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
    let envs = [
        ("BOT_PROFILES", profiles.to_str().unwrap()),
        ("BOT_DATABASE_URL", url.as_str()),
        ("BOT_SOCKET", socket.to_str().unwrap()),
    ];
    let server = Server(
        Command::new(BOT)
            .args(["serve", "--role", "gateway"])
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

    let first = bot(
        &envs,
        &[
            "chat",
            "--no-wait",
            "--request-id",
            "e2e-1",
            "오늘 일정 알려줘",
        ],
    );
    assert!(first.status.success(), "{}", text(&first));
    assert!(String::from_utf8_lossy(&first.stdout).contains("e2e-1"));
    // 같은 요청 ID·같은 글은 같은 접수, 접수는 한 건
    let again = bot(
        &envs,
        &[
            "chat",
            "--no-wait",
            "--request-id",
            "e2e-1",
            "오늘 일정 알려줘",
        ],
    );
    assert!(again.status.success(), "{}", text(&again));
    assert_eq!(schema.count("bot_runs"), "1");
    // 같은 ID에 다른 글은 충돌(재시도해도 소용없는 실패라 종료 코드 1)
    let conflict = bot(
        &envs,
        &["chat", "--no-wait", "--request-id", "e2e-1", "다른 내용"],
    );
    assert_eq!(conflict.status.code(), Some(1), "{}", text(&conflict));
    assert!(text(&conflict).contains("다른 내용이 왔어요"));
    // 접수 직후 끊겨도 같은 요청 ID로 재부착할 수 있다(새 접수 없음)
    let attach = bot(
        &envs,
        &["chat", "--attach", "--no-wait", "--request-id", "e2e-1"],
    );
    assert!(attach.status.success(), "{}", text(&attach));
    assert_eq!(schema.count("bot_runs"), "1");
    // 없는 요청 ID는 not_found
    let missing = bot(
        &envs,
        &["chat", "--attach", "--no-wait", "--request-id", "nope"],
    );
    assert_eq!(missing.status.code(), Some(1));
    drop(server);

    // 같은 폴더로 다시 시작하면 쓰기 없이 합류하고, 다른 폴더(판본)는 --activate 없이는 거절한다
    let changed = dir.path().join("profiles");
    copy_dir(&profiles, &changed);
    std::fs::write(changed.join("personal/agents/main/AGENT.md"), "다른 지침").unwrap();
    let refuse = bot(
        &[
            ("BOT_PROFILES", changed.to_str().unwrap()),
            ("BOT_DATABASE_URL", url.as_str()),
            ("BOT_SOCKET", socket.to_str().unwrap()),
        ],
        &["serve", "--role", "gateway"],
    );
    assert!(!refuse.status.success());
    assert!(text(&refuse).contains("--activate"), "{}", text(&refuse));
}
