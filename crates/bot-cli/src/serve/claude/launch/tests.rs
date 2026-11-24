use super::*;
use crate::host;
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
};

fn digest() -> String {
    "a".repeat(64)
}

fn host_with(apple: &str) -> HostFile {
    host::parse(&format!(
        r#"{{"version": 1, "models": {{"personal": ["assistant"]}},
            "providers": {{"assistant": {{"kind": "claude-cli", "model": "m", "endpoint": "https://api.example", "secret": "model-key"}}}}{apple}}}"#
    ))
    .unwrap()
}

/// 이 프로세스의 실효 uid(이 프로세스가 만든 폴더의 소유자)
fn me() -> u32 {
    let dir = tempfile::tempdir().unwrap();
    std::fs::metadata(dir.path()).unwrap().uid()
}

#[test]
fn the_relay_goes_where_the_anchor_confirmed_and_nowhere_else() {
    let gateway: Ipv4Addr = "192.168.64.1".parse().unwrap();
    // 생략하면 닻이 확인한 게이트웨이 IP의 빈 포트(0)
    assert_eq!(
        relay_target(gateway, None).unwrap(),
        "192.168.64.1:0".parse().unwrap()
    );
    // 적었으면 IP가 정확히 같을 때만(포트는 설정대로)
    for bind in ["192.168.64.1:9300", "192.168.64.1:0"] {
        let bind: SocketAddr = bind.parse().unwrap();
        assert_eq!(relay_target(gateway, Some(bind)).unwrap(), bind);
    }
    for bind in ["192.168.64.2:9300", "10.0.0.1:0", "192.168.65.1:9300"] {
        let error = relay_target(gateway, Some(bind.parse().unwrap())).unwrap_err();
        assert!(
            error.contains("192.168.64.1") && error.contains(bind),
            "{error}"
        );
    }
}

#[test]
fn the_host_config_becomes_a_firewalled_adapter_config_with_this_workers_name() {
    let apple = Apple {
        image: format!("img@sha256:{}", digest()),
        network: "bot-internal".into(),
        relay_bind: None,
        work_root: None,
        instance: Some("worker-a".into()),
        binary: Some("/opt/homebrew/bin/container".into()),
    };
    let config = container_config(&apple, Path::new("/var/bot/models"));
    assert_eq!(config.image, apple.image);
    assert_eq!(config.network, "bot-internal");
    assert_eq!(config.mount_root, PathBuf::from("/var/bot/models"));
    assert_eq!(
        (config.instance.as_str(), config.prefix.as_str()),
        ("worker-a", "worker-a")
    );
    // 운영은 신뢰 init(방화벽)이 있는 진입만 쓰고 방화벽 없이 시작하는 길은 없다
    assert!(config.entry.is_some() && !config.unfirewalled);
    assert_eq!(config.program, "/opt/homebrew/bin/container");
    // 이름·실행 파일을 생략하면 어댑터의 기본값(이름 `botvm`, PATH의 `container`)
    let default = container_config(
        &Apple {
            instance: None,
            binary: None,
            ..apple
        },
        Path::new("/m"),
    );
    assert_eq!(
        (default.instance.as_str(), default.program.as_str()),
        ("botvm", "container")
    );
}

#[test]
fn the_model_work_folder_is_private_to_this_user() {
    let dir = tempfile::tempdir().unwrap();
    let uid = me();
    // 없으면 만든다(0700, 중간 폴더도)
    let made = dir.path().join("state/work/models");
    private_dir(&made, uid).unwrap();
    assert_eq!(std::fs::metadata(&made).unwrap().mode() & 0o777, 0o700);
    // 이미 있는 0700 폴더는 그대로 쓴다
    private_dir(&made, uid).unwrap();
    // 다른 사용자도 읽을 수 있거나(0755) 파일이거나 링크이거나 소유자가 다르면 거절한다
    let open = dir.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(private_dir(&open, uid).unwrap_err().contains("0700"));
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    assert!(private_dir(&file, uid).is_err());
    let link = dir.path().join("link");
    symlink(&made, &link).unwrap();
    assert!(private_dir(&link, uid).is_err());
    assert!(private_dir(&made, uid.wrapping_add(1)).is_err());
}

#[test]
fn operational_parts_need_the_vm_config_and_never_fall_back_to_another_sandbox() {
    // sandbox.apple이 없으면 시작하지 않는다(srt 등으로 대신하지 않는다)
    let bare = host_with("");
    let error = operational(&bare, None, me()).err().unwrap();
    assert!(
        error.contains("sandbox.apple") && error.contains("claude-cli"),
        "{error}"
    );
    let srt_only =
        host_with(r#", "sandbox": {"srt": {"binary": "/usr/bin/true", "work_root": "/tmp/w"}}"#);
    assert!(
        operational(&srt_only, None, me())
            .err()
            .unwrap()
            .contains("sandbox.apple")
    );

    // 있으면 어댑터 설정·상류 전송·작업 폴더가 호스트 설정대로 만들어진다
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("models");
    let configured = host_with(&format!(
        r#", "sandbox": {{"apple": {{"image": "img@sha256:{}", "network": "bot-internal", "relay_bind": "192.168.64.1:9300",
            "work_root": "{}", "instance": "worker-b"}}}}"#,
        digest(),
        work.display()
    ));
    let parts = operational(&configured, None, me()).unwrap();
    assert_eq!(parts.workspace, work);
    assert_eq!(std::fs::metadata(&work).unwrap().mode() & 0o777, 0o700);
    let Launch::Container { config, relay_bind } = parts.launch else {
        panic!("운영은 VM 어댑터만 써요");
    };
    assert_eq!(relay_bind, Some("192.168.64.1:9300".parse().unwrap()));
    assert_eq!(
        (config.mount_root, config.instance.as_str()),
        (work, "worker-b")
    );
    assert!(parts.park.is_none());

    // 작업 폴더를 생략하면 설치 폴더 옆 `work/models`
    let installs = dir.path().join("state/installs");
    let defaults = host_with(&format!(
        r#", "sandbox": {{"apple": {{"image": "img@sha256:{}", "network": "n"}}}}"#,
        digest()
    ));
    let parts = operational(&defaults, Some(&installs), me()).unwrap();
    assert_eq!(parts.workspace, dir.path().join("state/work/models"));
    let Launch::Container { relay_bind, .. } = parts.launch else {
        panic!("운영은 VM 어댑터만 써요");
    };
    assert_eq!(relay_bind, None);
}
