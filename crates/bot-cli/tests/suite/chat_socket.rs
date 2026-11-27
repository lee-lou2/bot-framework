//! `bot chat`은 서버를 믿을 수 있을 때만 대화 글을 보낸다 — 실제 `bot` 프로세스로 확인한다.
//! (1) 다른 사용자가 드나들 수 있는 폴더(0777)나 공용 `/tmp` 바로 아래의 가짜 소켓에는 접속조차 하지 않고 원문을 보내지 않으며 명령은 실패로 끝난다.
//! (2) 런타임 폴더(`XDG_RUNTIME_DIR`·`TMPDIR`)가 없으면 공용 `/tmp`로 떨어지지 않고 홈 아래 개인 폴더를 쓰며, 홈도 없으면 이름을 밝혀 거절한다.
//! 저장소·모델은 필요 없다.
use crate::chat_support::*;
use crate::process_support::*;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

const SECRET: &str = "비밀 대화 내용 7731";

/// 환경을 비우고 `PATH`와 주어진 값만 넘겨 돌린다(`HOME`·`TMPDIR` 같은 값이 시험을 돌리는 쪽 것으로 새지 않게).
/// 믿을 수 없는 서버에 접속해 응답을 기다리며 멈추는 경우(점검이 빠진 구현)에도 시험이 멈추지 않게 20초 안에 끝나지 않으면 죽이고 실패한다.
fn bot_with(envs: &[(&str, &str)], args: &[&str]) -> Output {
    let mut child = Command::new(BOT)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(envs.iter().copied())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("bot 실행");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("bot chat이 끝나지 않았어요(믿을 수 없는 서버에 접속해 기다린 것으로 보여요)");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("k")
        .tempdir_in("/tmp")
        .unwrap()
}

fn assert_refused(output: &Output, server: Seen) {
    let shown = text(output);
    // 접속하지도 원문을 보내지도 않았다. 다시 시도해도 같으니 재시도 코드(3)가 아니라 실패(1)다
    assert_eq!(server.connections, 0, "접속하지 않았어요: {shown}");
    assert_eq!(server.request, "", "{shown}");
    assert_eq!(output.status.code(), Some(1), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(
        shown.contains("대화 글을 보내지 않았어요") && shown.contains("소켓 폴더"),
        "{shown}"
    );
    assert!(
        !shown.contains("연결이 끊겼어요"),
        "재시도를 권하지 않아요: {shown}"
    );
}

#[test]
fn a_fake_socket_in_a_folder_others_can_enter_gets_no_conversation_and_the_command_fails() {
    for mode in [0o777, 0o755, 0o750] {
        let dir = scratch();
        let socket = dir.path().join("shared").join("g.sock");
        let server = fake_gateway(&socket, mode, vec![json!({"type": "accepted"})]);
        let output = bot_with(
            &[("BOT_SOCKET", socket.to_str().unwrap())],
            &["chat", "--request-id", "k1", SECRET],
        );
        assert_refused(&output, server.join().unwrap());
    }
}

#[test]
fn a_fake_socket_straight_under_the_shared_tmp_is_not_trusted_either() {
    // 기본 위치가 `/tmp/bot-<uid>`로 떨어지던 때의 모양: 폴더가 내 것이 아니다(루트 소유·스티키)
    let socket = PathBuf::from(format!("/tmp/botchat-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let output = bot_with(
        &[("BOT_SOCKET", socket.to_str().unwrap())],
        &["chat", "--request-id", "k2", SECRET],
    );
    let connected = listener.accept().is_ok();
    let _ = std::fs::remove_file(&socket);
    let shown = text(&output);
    assert!(!connected, "접속하지 않았어요: {shown}");
    assert_eq!(output.status.code(), Some(1), "{shown}");
    assert!(shown.contains("대화 글을 보내지 않았어요"), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
}

#[test]
fn a_private_folder_is_trusted_and_the_conversation_goes_through() {
    // 대조: 내 비공개(0700) 폴더의 서버에는 정상으로 간다
    let dir = scratch();
    let socket = dir.path().join("run").join("g.sock");
    let server = fake_gateway(
        &socket,
        0o700,
        vec![json!({"type": "result", "state": "succeeded", "result": "안녕하세요"})],
    );
    let output = bot_with(
        &[("BOT_SOCKET", socket.to_str().unwrap())],
        &["chat", "--request-id", "k3", SECRET],
    );
    let seen = server.join().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    assert!(seen.request.contains(SECRET), "{}", seen.request);
    assert!(String::from_utf8_lossy(&output.stdout).contains("안녕하세요"));
}

/// 홈 아래 기본 위치(`$HOME/.local/state/bot/run/gateway.sock`)
fn home_socket(home: &Path) -> PathBuf {
    home.join(".local/state/bot/run/gateway.sock")
}

#[test]
fn without_a_runtime_folder_the_default_is_under_home_never_the_shared_tmp() {
    let home = scratch();
    let home_str = home.path().to_str().unwrap();
    // 서버가 없다: 안내하는 위치는 홈 아래다(`/tmp/bot-…`가 아니다)
    let none = bot_with(&[("HOME", home_str)], &["chat", "안녕"]);
    let shown = text(&none);
    assert_eq!(none.status.code(), Some(1), "{shown}");
    assert!(shown.contains("bot serve가 실행 중인지"), "{shown}");
    assert!(
        shown.contains(home_socket(home.path()).to_str().unwrap()),
        "{shown}"
    );
    assert!(!shown.contains("/tmp/bot-"), "{shown}");

    // 같은 기본 위치의 서버(내 비공개 폴더)에는 `BOT_SOCKET` 없이도 정상으로 간다
    let socket = home_socket(home.path());
    let server = fake_gateway(
        &socket,
        0o700,
        vec![json!({"type": "result", "state": "succeeded", "result": "홈의 서버예요"})],
    );
    let output = bot_with(
        &[("HOME", home_str)],
        &["chat", "--request-id", "k4", SECRET],
    );
    let seen = server.join().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    assert!(seen.request.contains(SECRET));
    assert!(String::from_utf8_lossy(&output.stdout).contains("홈의 서버예요"));

    // 런타임 폴더(TMPDIR)가 있으면 그쪽이 먼저다
    let runtime = scratch();
    let socket = runtime
        .path()
        .join(format!("bot-{}", my_uid()))
        .join("gateway.sock");
    let server = fake_gateway(
        &socket,
        0o700,
        vec![json!({"type": "result", "state": "succeeded", "result": "런타임 폴더의 서버예요"})],
    );
    let output = bot_with(
        &[
            ("HOME", home_str),
            ("TMPDIR", runtime.path().to_str().unwrap()),
        ],
        &["chat", "--request-id", "k5", "안녕"],
    );
    let _ = server.join().unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("런타임 폴더의 서버예요"));
}

#[test]
fn with_no_runtime_folder_and_no_usable_home_the_location_is_refused_by_name() {
    for envs in [
        vec![],
        vec![("HOME", "")],
        vec![("HOME", "relative/home")],
        vec![("TMPDIR", ""), ("XDG_RUNTIME_DIR", "")],
    ] {
        let output = bot_with(&envs, &["chat", "--request-id", "k6", SECRET]);
        let shown = text(&output);
        assert_eq!(output.status.code(), Some(1), "{envs:?}: {shown}");
        assert!(
            shown.contains("BOT_SOCKET") && shown.contains("HOME"),
            "{envs:?}: {shown}"
        );
        assert!(!shown.contains("/tmp/bot-"), "{shown}");
        assert!(!shown.contains(SECRET), "{shown}");
    }
}
