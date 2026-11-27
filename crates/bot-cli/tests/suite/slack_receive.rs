//! Slack `receive=http`는 아직 받아 줄 수신기가 없다: 서명 비밀을 줘도 메시지·승인 버튼을 받지 못하고 서명·재전송 검사도 동작하지 않는다. 그래서 설정 검증이
//! 워크스페이스 이름을 밝혀 거절한다 — `bot validate`도 `bot serve`도(아무것도 시작하지 않는다). 소켓 수신은 같은 설정에서 그대로 통과한다.
//! 실제 `bot` 프로세스를 쓰며 DB·Slack·claude는 필요 없다.
use crate::process_support::*;
use std::path::Path;

/// 서명 비밀까지 갖춘 Slack 워크스페이스 `home`(수신 방식만 다르다)을 담은 호스트 설정
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
    // 대조: 같은 설정이 소켓 수신이면 통과한다(거절 이유가 수신 방식 하나뿐임을 보인다)
    let socket_host = host(dir.path(), "");
    let ok = bot(&refs(&env_for_host(&socket_host)), &["validate"]);
    assert!(ok.status.success(), "{}", text(&ok));

    let http_host = host(dir.path(), r#", "receive": "http""#);
    let env = env_for_host(&http_host);
    // `bot validate`: 통과를 알리지 않고 워크스페이스·필드 이름과 이유를 보여 준다
    let validate = bot(&refs(&env), &["validate"]);
    assert!(!validate.status.success(), "{}", text(&validate));
    let message = text(&validate);
    assert!(
        message.contains("slack.workspaces.home.receive") && message.contains("http 수신"),
        "{message}"
    );
    assert!(!message.contains("통과"), "{message}");
    // `bot serve`: 같은 이유로 시작하지 않는다(소켓도 열지 않는다)
    let serve = bot(&refs(&env), &["serve", "--role", "gateway"]);
    assert!(!serve.status.success(), "{}", text(&serve));
    let message = text(&serve);
    assert!(
        message.contains("slack.workspaces.home.receive") && message.contains("http 수신"),
        "{message}"
    );
    assert!(!socket.exists(), "아무것도 시작하지 않아야 해요");
}
