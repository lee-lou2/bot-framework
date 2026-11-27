//! Verifies that Slack `receive=http` is rejected by workspace/field name in both `bot validate`
//! and `bot serve` before opening any socket, while `receive=socket` passes (no DB/Slack/claude needed).
use crate::process_support::*;
use std::path::Path;

fn host(dir: &Path, receive: &str) -> std::path::PathBuf {
    let pkg = package(dir, "#!/bin/sh\nexit 0\n");
    let slack = format!(
        r#", "slack": {{"workspaces": {{"home": {{"team_id": "T1", "bot_token": "bot-token", "app_token": "app-token",
            "signing_secret": "signing-secret", "dm": true{receive}}}}}}}"#
    );
    let path = dir.join(format!("host-{}.json", receive.len()));
    std::fs::write(
        &path,
        host_json(
            my_uid(),
            &pkg,
            &slack,
            r#", "providers": {"assistant": {"kind": "demo"}}"#,
        ),
    )
    .unwrap();
    path
}

#[test]
fn http_receiving_is_refused_by_name_before_anything_starts_and_socket_receiving_passes() {
    let dir = tempfile::Builder::new()
        .prefix("r")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let env_for_host = |host: &Path| {
        env_for(
            &examples(),
            "postgres://unused@localhost/none",
            &socket,
            &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
        )
    };
    let socket_host = host(dir.path(), "");
    let ok = bot(&refs(&env_for_host(&socket_host)), &["validate"]);
    assert!(ok.status.success(), "{}", text(&ok));

    let http_host = host(dir.path(), r#", "receive": "http""#);
    let env = env_for_host(&http_host);
    let validate = bot(&refs(&env), &["validate"]);
    assert!(!validate.status.success(), "{}", text(&validate));
    let message = text(&validate);
    assert!(
        message.contains("slack.workspaces.home.receive") && message.contains("http receive"),
        "{message}"
    );
    assert!(
        !message.to_ascii_lowercase().contains("passed"),
        "{message}"
    );
    let serve = bot(&refs(&env), &["serve", "--role", "gateway"]);
    assert!(!serve.status.success(), "{}", text(&serve));
    let message = text(&serve);
    assert!(
        message.contains("slack.workspaces.home.receive") && message.contains("http receive"),
        "{message}"
    );
    assert!(!socket.exists(), "must not start or bind socket");
}
