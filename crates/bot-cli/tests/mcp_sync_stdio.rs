//! End-to-end sandboxed stdio `bot mcp sync` test against `examples/servers/demo-agenda`.

use bot_broker::testing::FakeSecrets;
use bot_cli::{mcp_sync, settings::Settings};
use bot_core::port::SecretStore;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

fn find_srt() -> Option<PathBuf> {
    std::env::split_paths(std::env::var_os("PATH").as_deref()?)
        .map(|d| d.join("srt"))
        .find(|p| p.is_absolute() && p.is_file())
}

fn demo_agenda_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/servers/demo-agenda")
        .canonicalize()
        .expect("demo-agenda server directory must exist")
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[tokio::test]
async fn sandboxed_stdio_mcp_sync_locks_demo_agenda_deterministically_and_reports_early_exit() {
    let Some(srt_bin) = find_srt() else {
        if std::env::var("BOT_SRT_CANARY").as_deref() == Ok("required") {
            panic!("BOT_SRT_CANARY=required but srt is not on PATH");
        }
        eprintln!("skipped: srt not on PATH");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let profiles = tmp.path().join("profiles");
    let src_profile =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles/personal");
    copy_dir(&src_profile, &profiles.join("personal"));
    let skill = profiles.join("personal/skills/agenda-sync");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: agenda-sync\ndescription: Agenda tools\n---\nUse agenda tools.\n",
    )
    .unwrap();
    std::fs::write(
        skill.join("tools.yaml"),
        "version: 1\nserver: demo-agenda\ntools:\n  - id: list\n    class: read\n    scopes: [agenda.read]\n  - id: delete\n    class: irreversible\n    scopes: [agenda.write]\n",
    )
    .unwrap();

    // Also stage a broken stdio server that exits immediately to test `server_exit_before_list`.
    let broken_root = tmp.path().join("broken-server");
    std::fs::create_dir_all(broken_root.join("bin")).unwrap();
    let broken_bin = broken_root.join("bin/fail");
    std::fs::write(
        &broken_bin,
        "#!/bin/sh\necho 'secret-ish stderr' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&broken_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let installs = tmp.path().join("installs");
    let work = tmp.path().join("work");
    let host_path = tmp.path().join("host.json");
    let host_json = serde_json::json!({
        "version": 1,
        "servers": {
            "demo-agenda": {
                "root": demo_agenda_root().display().to_string(),
                "command": ["bin/server", "--stdio"]
            },
            "broken": {
                "root": broken_root.display().to_string(),
                "command": ["bin/fail"]
            }
        },
        "sandbox": {
            "srt": {
                "binary": srt_bin.display().to_string(),
                "work_root": work.display().to_string()
            }
        }
    });
    std::fs::write(&host_path, host_json.to_string()).unwrap();

    let settings = Settings::read(&|key| match key {
        "BOT_PROFILES" => Some(profiles.display().to_string()),
        "BOT_HOST_CONFIG" => Some(host_path.display().to_string()),
        "BOT_INSTALLS" => Some(installs.display().to_string()),
        _ => None,
    })
    .unwrap();

    let make_store =
        || -> Result<Arc<dyn SecretStore>, String> { Ok(Arc::new(FakeSecrets::new(&[]))) };

    // 1. Sync demo-agenda inside SrtSandbox.
    mcp_sync::run_with(&settings, &skill, make_store)
        .await
        .expect("stdio mcp sync on demo-agenda must succeed");
    let lock_path = skill.join("tools.lock.yaml");
    let first = std::fs::read_to_string(&lock_path).unwrap();
    assert!(first.contains("server: \"demo-agenda\""), "{first}");
    assert!(first.contains("id: \"list\""), "{first}");
    assert!(first.contains("id: \"delete\""), "{first}");
    assert!(first.contains("sha256:"), "{first}");

    // 2. Re-running produces byte-identical lock output.
    mcp_sync::run_with(&settings, &skill, make_store)
        .await
        .expect("second sync must succeed");
    assert_eq!(std::fs::read_to_string(&lock_path).unwrap(), first);

    // 3. Early-exiting stdio server reports fixed diagnostic without echoing untrusted stderr.
    let broken_skill = profiles.join("personal/skills/broken");
    std::fs::create_dir_all(&broken_skill).unwrap();
    std::fs::write(
        broken_skill.join("SKILL.md"),
        "---\nname: broken\ndescription: Broken\n---\nBroken.\n",
    )
    .unwrap();
    std::fs::write(
        broken_skill.join("tools.yaml"),
        "version: 1\nserver: broken\ntools:\n  - id: run\n    class: read\n    scopes: [agenda.read]\n",
    )
    .unwrap();
    let err = mcp_sync::run_with(&settings, &broken_skill, make_store)
        .await
        .unwrap_err();
    assert!(err.contains("server_exit_before_list"), "{err}");
    assert!(
        !err.contains("secret-ish stderr"),
        "must never echo stderr: {err}"
    );
}
