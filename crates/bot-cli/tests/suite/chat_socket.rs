//! Process-level tests for `bot chat` socket trust and default path resolution: refuses sockets
//! in shared or non-owner directories without connecting or sending text, and never falls back to `/tmp`.
use crate::chat_support::*;
use crate::process_support::*;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

const SECRET: &str = "secret conversation text 7731";

/// Runs `bot` with a clean environment (`PATH` + `envs`) and kills it after 20s if it hangs on an untrusted socket.
fn bot_with(envs: &[(&str, &str)], args: &[&str]) -> Output {
    let mut child = Command::new(BOT)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(envs.iter().copied())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn bot");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("bot chat hung (likely connected to an untrusted socket)");
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
    let low = shown.to_ascii_lowercase();
    // Non-retryable failure (exit 1): never connected and never sent the message.
    assert_eq!(server.connections, 0, "must not connect: {shown}");
    assert_eq!(server.request, "", "{shown}");
    assert_eq!(output.status.code(), Some(1), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(
        shown.contains("socket directory")
            && (low.contains("not sent")
                || low.contains("did not send")
                || low.contains("without sending")
                || low.contains("not send")),
        "{shown}"
    );
    assert!(
        !shown.contains("--attach") && !low.contains("disconnected"),
        "must not suggest retry: {shown}"
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
    let low = shown.to_ascii_lowercase();
    assert!(!connected, "must not connect: {shown}");
    assert_eq!(output.status.code(), Some(1), "{shown}");
    assert!(
        shown.contains("socket directory")
            && (low.contains("not sent")
                || low.contains("did not send")
                || low.contains("without sending")
                || low.contains("not send")),
        "{shown}"
    );
    assert!(!shown.contains(SECRET), "{shown}");
}

#[test]
fn a_private_folder_is_trusted_and_the_conversation_goes_through() {
    let dir = scratch();
    let socket = dir.path().join("run").join("g.sock");
    let server = fake_gateway(
        &socket,
        0o700,
        vec![json!({"type": "result", "state": "succeeded", "result": "Hello there"})],
    );
    let output = bot_with(
        &[("BOT_SOCKET", socket.to_str().unwrap())],
        &["chat", "--request-id", "k3", SECRET],
    );
    let seen = server.join().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    assert!(seen.request.contains(SECRET), "{}", seen.request);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Hello there"));
}

fn home_socket(home: &Path) -> PathBuf {
    home.join(".local/state/bot/run/gateway.sock")
}

#[test]
fn without_a_runtime_folder_the_default_is_under_home_never_the_shared_tmp() {
    let home = scratch();
    let home_str = home.path().to_str().unwrap();
    let none = bot_with(&[("HOME", home_str)], &["chat", "Hello"]);
    let shown = text(&none);
    assert_eq!(none.status.code(), Some(1), "{shown}");
    assert!(shown.contains("bot serve"), "{shown}");
    assert!(
        shown.contains(home_socket(home.path()).to_str().unwrap()),
        "{shown}"
    );
    assert!(!shown.contains("/tmp/bot-"), "{shown}");

    let socket = home_socket(home.path());
    let server = fake_gateway(
        &socket,
        0o700,
        vec![json!({"type": "result", "state": "succeeded", "result": "Server under home"})],
    );
    let output = bot_with(
        &[("HOME", home_str)],
        &["chat", "--request-id", "k4", SECRET],
    );
    let seen = server.join().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    assert!(seen.request.contains(SECRET));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Server under home"));

    // Runtime directory (`TMPDIR`) takes precedence over `HOME`.
    let runtime = scratch();
    let socket = runtime
        .path()
        .join(format!("bot-{}", my_uid()))
        .join("gateway.sock");
    let server = fake_gateway(
        &socket,
        0o700,
        vec![json!({"type": "result", "state": "succeeded", "result": "Server under runtime dir"})],
    );
    let output = bot_with(
        &[
            ("HOME", home_str),
            ("TMPDIR", runtime.path().to_str().unwrap()),
        ],
        &["chat", "--request-id", "k5", "Hello"],
    );
    let _ = server.join().unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("Server under runtime dir"));
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
