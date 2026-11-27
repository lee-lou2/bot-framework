//! Process-level tests enforcing `remote: true` for plaintext OTLP collector endpoints outside loopback/private networks.
use crate::process_support::*;
use std::path::{Path, PathBuf};

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
    let (_dir, host, socket) = setup(
        r#"{"endpoint": "http://collector.example.com:4318/v1/private-path", "headers": {"X-Env": "plain-header-value-1"}}"#,
    );

    // `bot validate` reports profile, host, and `remote` key without leaking path, port, or header values.
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
    assert!(!message.contains("Validation passed"), "{message}");

    // `bot serve` fails host validation before checking DB URL or opening the CLI socket.
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
        "host check must run before DB connection: {message}"
    );
    assert!(!socket.exists(), "CLI socket must not be opened");
}

#[test]
fn naming_remote_lets_the_check_through_and_private_addresses_need_no_option() {
    let (_dir, host, socket) =
        setup(r#"{"endpoint": "http://collector.example.com:4318/v1/traces", "remote": true}"#);
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
    assert!(!message.contains("admin.trace"), "{message}");

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
