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

fn me() -> u32 {
    let dir = tempfile::tempdir().unwrap();
    std::fs::metadata(dir.path()).unwrap().uid()
}

#[test]
fn the_relay_goes_where_the_anchor_confirmed_and_nowhere_else() {
    let gateway: Ipv4Addr = "192.168.64.1".parse().unwrap();
    assert_eq!(
        relay_target(gateway, None).unwrap(),
        "192.168.64.1:0".parse().unwrap()
    );
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
    assert!(config.entry.is_some() && !config.unfirewalled);
    assert_eq!(config.program, "/opt/homebrew/bin/container");
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
    let made = dir.path().join("state/work/models");
    private_dir(&made, uid).unwrap();
    assert_eq!(std::fs::metadata(&made).unwrap().mode() & 0o777, 0o700);
    private_dir(&made, uid).unwrap();
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
        panic!("production must use the VM adapter");
    };
    assert_eq!(relay_bind, Some("192.168.64.1:9300".parse().unwrap()));
    assert_eq!(
        (config.mount_root, config.instance.as_str()),
        (work, "worker-b")
    );
    assert!(parts.park.is_none());

    let installs = dir.path().join("state/installs");
    let defaults = host_with(&format!(
        r#", "sandbox": {{"apple": {{"image": "img@sha256:{}", "network": "n"}}}}"#,
        digest()
    ));
    let parts = operational(&defaults, Some(&installs), me()).unwrap();
    assert_eq!(parts.workspace, dir.path().join("state/work/models"));
    let Launch::Container { relay_bind, .. } = parts.launch else {
        panic!("production must use the VM adapter");
    };
    assert_eq!(relay_bind, None);
}
