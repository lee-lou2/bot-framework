//! Verifies non-loopback plain HTTP `gateway.http.listen` requires explicit `gateway.http.remote: true`
//! in both `bot validate` and `bot serve`, before connecting to PostgreSQL or opening any socket.
use crate::process_support::*;
use std::{
    net::TcpStream,
    path::{Path, PathBuf},
    time::Duration,
};

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

fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)).is_ok()
}

#[test]
fn a_plain_http_entrance_beyond_loopback_is_refused_by_name_unless_the_operator_says_so() {
    let port = free_port();
    let open = format!(r#"{{"listen": "0.0.0.0:{port}"}}"#);
    let (_dir, host, socket) = setup(&open);

    let validate = run(&host, &socket, &["validate"]);
    let message = text(&validate);
    assert!(!validate.status.success(), "{message}");
    assert!(
        message.contains("gateway.http.listen") && message.contains("http.remote"),
        "{message}"
    );
    assert!(!message.contains("Validation passed"), "{message}");

    // `bot serve` must reject the host config before checking DB or opening any socket.
    let serve = run(&host, &socket, &["serve", "--role", "gateway"]);
    let message = text(&serve);
    assert!(!serve.status.success(), "{message}");
    assert!(
        message.contains("gateway.http.listen") && message.contains("http.remote"),
        "{message}"
    );
    assert!(
        !message.contains("BOT_DATABASE_URL"),
        "host check must run before DB check: {message}"
    );
    assert!(!socket.exists(), "CLI socket must not be opened");
    assert!(!listening(port), "{port}: HTTP listener must not be opened");
}

#[test]
fn naming_remote_lets_the_check_through_and_loopback_needs_no_flag() {
    let port = free_port();
    let (_dir, host, socket) = setup(&format!(
        r#"{{"listen": "0.0.0.0:{port}", "remote": true}}"#
    ));
    let validate = run(&host, &socket, &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
    assert!(
        text(&validate).contains("Validation passed"),
        "{}",
        text(&validate)
    );
    let serve = run(&host, &socket, &["serve", "--role", "gateway"]);
    let message = text(&serve);
    assert!(!serve.status.success(), "{message}");
    assert!(message.contains("BOT_DATABASE_URL"), "{message}");
    assert!(!message.contains("gateway.http.listen"), "{message}");

    let (_dir, host, socket) = setup(&format!(r#"{{"listen": "127.0.0.1:{port}"}}"#));
    let validate = run(&host, &socket, &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
}
