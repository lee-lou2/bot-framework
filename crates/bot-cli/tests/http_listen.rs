//! 평문 HTTP 입구(`gateway.http.listen`)는 루프백이 아닌 주소를 `gateway.http.remote: true`로 명시할 때만 연다 — 실제 `bot` 프로세스로 확인한다:
//! `bot validate`와 `bot serve`가 필드 이름(`gateway.http.listen`)과 고치는 법(`http.remote`)을 밝혀 거절하고(아무것도 열지 않는다), 명시하면 이 점검을 지난다.
//! 저장소·Slack에는 접속하지 않는다(호스트 파일 점검이 저장소 접속보다 먼저다).
mod process_support;
use process_support::*;
use std::{
    net::TcpStream,
    path::{Path, PathBuf},
    time::Duration,
};

/// 호스트 파일(`gateway.http` 절만 바꾼다)과 그 설정으로 `bot`을 돌릴 환경
fn setup(http: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("h")
        .tempdir_in("/tmp")
        .unwrap();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(my_uid(), &pkg, &format!(r#", "http": {http}"#), ""),
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

/// 이 포트에 지금 누가 듣고 있는가
fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)).is_ok()
}

#[test]
fn a_plain_http_entrance_beyond_loopback_is_refused_by_name_unless_the_operator_says_so() {
    let port = free_port();
    let open = format!(r#"{{"listen": "0.0.0.0:{port}"}}"#);
    let (_dir, host, socket) = setup(&open);

    // `bot validate`: 필드 이름과 고치는 법을 밝혀 거절한다
    let validate = run(&host, &socket, &["validate"]);
    let message = text(&validate);
    assert!(!validate.status.success(), "{message}");
    assert!(
        message.contains("gateway.http.listen") && message.contains("http.remote"),
        "{message}"
    );
    assert!(!message.contains("검증 통과"), "{message}");

    // `bot serve`: 저장소에 접속하기도 전에 같은 이유로 멈추고, 아무것도 열지 않는다
    let serve = run(&host, &socket, &["serve", "--role", "gateway"]);
    let message = text(&serve);
    assert!(!serve.status.success(), "{message}");
    assert!(
        message.contains("gateway.http.listen") && message.contains("http.remote"),
        "{message}"
    );
    assert!(
        !message.contains("BOT_DATABASE_URL"),
        "호스트 점검이 저장소 접속보다 먼저예요: {message}"
    );
    assert!(!socket.exists(), "CLI 소켓도 열지 않았어요");
    assert!(!listening(port), "{port}: HTTP 입구도 열지 않았어요");
}

#[test]
fn naming_remote_lets_the_check_through_and_loopback_needs_no_flag() {
    let port = free_port();
    // 명시하면 점검을 지난다: validate는 통과하고, serve는 호스트 점검을 지나 저장소 주소를 요구하는 데까지 간다(이 시험은 저장소에 접속하지 않는다)
    let (_dir, host, socket) = setup(&format!(
        r#"{{"listen": "0.0.0.0:{port}", "remote": true}}"#
    ));
    let validate = run(&host, &socket, &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
    assert!(text(&validate).contains("검증 통과"), "{}", text(&validate));
    let serve = run(&host, &socket, &["serve", "--role", "gateway"]);
    let message = text(&serve);
    assert!(!serve.status.success(), "{message}");
    assert!(message.contains("BOT_DATABASE_URL"), "{message}");
    assert!(!message.contains("gateway.http.listen"), "{message}");

    // 루프백은 아무 표시 없이 열린다
    let (_dir, host, socket) = setup(&format!(r#"{{"listen": "127.0.0.1:{port}"}}"#));
    let validate = run(&host, &socket, &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
}
