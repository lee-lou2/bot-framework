//! 평문 OTLP 수집기(`admin.trace.<프로필>.endpoint`)는 루프백·사설 주소만 명시 없이 받는다 — 실제 `bot` 프로세스로 확인한다: 헤더 비밀과 내용이 평문으로 나가므로
//! `bot validate`와 `bot serve`가 프로필 이름·호스트·고치는 법(`remote`)을 밝혀 거절하고(저장소에 접속하기도 전에 멈춘다), 명시하면 이 점검을 지난다.
use crate::process_support::*;
use std::path::{Path, PathBuf};

/// 호스트 파일(`admin.trace.personal` 절만 바꾼다)과 그 설정으로 `bot`을 돌릴 소켓 경로
fn setup(personal: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("t")
        .tempdir_in("/tmp")
        .unwrap();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            "",
            &format!(
                r#", "admin": {{"bind": "127.0.0.1:0", "trace": {{"personal": {personal}}}}}"#
            ),
        ),
    )
    .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    (dir, host, socket)
}

fn run(host: &Path, socket: &Path, args: &[&str]) -> std::process::Output {
    let profiles = examples();
    bot(
        &[
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_SOCKET", socket.to_str().unwrap()),
        ],
        args,
    )
}

#[test]
fn a_collector_beyond_the_private_network_is_refused_by_name_unless_the_operator_says_so() {
    // 평문 http라 헤더(값)와 내용이 그대로 나간다: 공개 주소의 수집기를 명시 없이 쓰려 한다
    let (_dir, host, socket) = setup(
        r#"{"endpoint": "http://collector.example.com:4318/v1/private-path", "headers": {"X-Env": "plain-header-value-1"}}"#,
    );

    // `bot validate`: 프로필 이름·호스트·고치는 법을 밝혀 거절한다(경로·헤더 값·포트는 싣지 않는다)
    let validate = run(&host, &socket, &["validate"]);
    let message = text(&validate);
    assert!(!validate.status.success(), "{message}");
    for shown in [
        "personal",
        "collector.example.com",
        "admin.trace.personal.remote",
    ] {
        assert!(message.contains(shown), "{shown}: {message}");
    }
    for hidden in ["private-path", "plain-header-value-1", "4318"] {
        assert!(!message.contains(hidden), "{hidden}: {message}");
    }
    assert!(!message.contains("검증 통과"), "{message}");

    // `bot serve`: 저장소에 접속하기도 전에 같은 이유로 멈추고, 아무것도 열지 않는다
    let serve = run(&host, &socket, &["serve", "--role", "gateway"]);
    let message = text(&serve);
    assert!(!serve.status.success(), "{message}");
    assert!(
        message.contains("collector.example.com")
            && message.contains("admin.trace.personal.remote"),
        "{message}"
    );
    assert!(
        !message.contains("BOT_DATABASE_URL"),
        "호스트 점검이 저장소 접속보다 먼저예요: {message}"
    );
    assert!(!socket.exists(), "CLI 소켓도 열지 않았어요");
}

#[test]
fn naming_remote_lets_the_check_through_and_private_addresses_need_no_option() {
    // 명시하면 점검을 지난다: validate는 통과하고, serve는 호스트 점검을 지나 저장소 주소를 요구하는 데까지 간다(이 시험은 저장소에 접속하지 않는다)
    let (_dir, host, socket) =
        setup(r#"{"endpoint": "http://collector.example.com:4318/v1/traces", "remote": true}"#);
    let validate = run(&host, &socket, &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
    assert!(text(&validate).contains("검증 통과"), "{}", text(&validate));
    let serve = run(&host, &socket, &["serve", "--role", "gateway"]);
    let message = text(&serve);
    assert!(!serve.status.success(), "{message}");
    assert!(message.contains("BOT_DATABASE_URL"), "{message}");
    assert!(!message.contains("admin.trace"), "{message}");

    // 루프백·사설 주소·localhost는 아무 표시 없이 받는다
    for host_name in ["127.0.0.1", "[::1]", "localhost", "10.1.2.3", "192.168.0.7"] {
        let (_dir, host, socket) = setup(&format!(
            r#"{{"endpoint": "http://{host_name}:4318/v1/traces"}}"#
        ));
        let validate = run(&host, &socket, &["validate"]);
        assert!(
            validate.status.success(),
            "{host_name}: {}",
            text(&validate)
        );
    }

    // `remote`는 true·false여야 한다
    let (_dir, host, socket) =
        setup(r#"{"endpoint": "http://127.0.0.1:4318/v1/traces", "remote": "yes"}"#);
    let validate = run(&host, &socket, &["validate"]);
    assert!(!validate.status.success());
    assert!(
        text(&validate).contains("admin.trace.personal.remote"),
        "{}",
        text(&validate)
    );
}
