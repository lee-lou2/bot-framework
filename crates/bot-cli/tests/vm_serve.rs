//! Real Apple `container` VM acceptance tests for `serve`: anchor VM, broker front relay, real `claude` inside model VM (fake upstream), and VM cleanup on stop or startup failure.
//! Run: `BOT_CONTAINER_CANARY=required BOT_MODEL_IMAGE=<name@sha256:...> BOT_TEST_DATABASE_URL=... cargo test -p bot-cli --test vm_serve -- --nocapture`.
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use claude_support::*;
use process_support::Schema;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run]
members:
  - {identity: "local:owner", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
secrets: []
policy:
  scopes: [run]
  approve: []
"#;

fn container_bin() -> Option<PathBuf> {
    ["/opt/homebrew/bin/container", "/usr/local/bin/container"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

fn ok(program: &Path, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn environment() -> Option<(PathBuf, String)> {
    let required = std::env::var("BOT_CONTAINER_CANARY").as_deref() == Ok("required");
    let found = container_bin()
        .filter(|program| ok(program, &["system", "status"]))
        .and_then(|program| {
            let image = std::env::var("BOT_MODEL_IMAGE")
                .ok()
                .filter(|image| image.contains("@sha256:"))?;
            ok(&program, &["image", "inspect", &image]).then_some((program, image))
        });
    if found.is_none() {
        assert!(
            !required,
            "BOT_CONTAINER_CANARY=required: requires container CLI/service and local image BOT_MODEL_IMAGE=<name@sha256:...> (images/model/build.sh)"
        );
        eprintln!("skipped: container CLI/service or local BOT_MODEL_IMAGE not available");
    }
    found
}

fn ids(program: &Path, prefix: &str) -> Vec<String> {
    let output = Command::new(program)
        .args(["ls", "--all", "--format", "json"])
        .output()
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["configuration"]["id"].as_str())
        .filter(|id| id.starts_with(prefix))
        .map(str::to_owned)
        .collect()
}

/// Waits up to 30s for container count to reach `want` because `--rm` deletion of exited model VMs is asynchronous.
async fn settle(program: &Path, prefix: &str, want: usize) -> Vec<String> {
    let mut seen = ids(program, prefix);
    for _ in 0..150 {
        if seen.len() == want {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        seen = ids(program, prefix);
    }
    seen
}

/// Machine-wide lock (`/tmp/bot-vm-canary.lock`) serializing real VM tests across processes to avoid network/startup contention.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    static MACHINE: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();
    let guard = ONE.lock().await;
    if MACHINE.get().is_none() {
        let file = tokio::task::spawn_blocking(|| {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/bot-vm-canary.lock")
                .expect("VM test lock file");
            for _ in 0..4500 {
                if file.try_lock().is_ok() {
                    return file;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            eprintln!("warning: another VM test held the lock for 15m; proceeding unlocked");
            file
        })
        .await
        .unwrap();
        let _ = MACHINE.set(file);
    }
    guard
}

struct Cleanup {
    program: PathBuf,
    instance: String,
    network: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for id in ids(&self.program, &format!("{}-", self.instance)) {
            ok(&self.program, &["kill", &id]);
            ok(&self.program, &["delete", "--force", &id]);
        }
        ok(&self.program, &["network", "delete", &self.network]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_real_claude_in_the_model_vm_answers_through_the_host_front_and_stopping_serve_cleans_up_the_vms()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let Some((program, image)) = environment() else {
        return;
    };
    let _serial = serial().await;
    let pid = std::process::id();
    let cleanup = Cleanup {
        program: program.clone(),
        instance: format!("botvm-s{pid}"),
        network: format!("botvm-canary-{pid}-serve"),
    };
    let rig = Rig::start_vm(
        schema,
        PROFILE,
        &cli_gateway(),
        &[Role::Gateway, Role::Worker],
        Vm {
            image,
            network: cleanup.network.clone(),
            instance: cleanup.instance.clone(),
            relay_bind: None,
        },
    )
    .await;
    assert_eq!(
        ids(&program, &format!("{}-", cleanup.instance)).len(),
        1,
        "one anchor VM must be running"
    );

    rig.script(final_turn("msg_vm1", "Model inside VM replied."));
    let out = rig.chat("vm1", "hello").await;
    assert!(out.status.success(), "{}", said(&out));
    assert!(
        said(&out).contains("Model inside VM replied."),
        "{}",
        said(&out)
    );
    assert_eq!(
        rig.upstream.requests().len(),
        1,
        "expected exactly one upstream request"
    );
    let left = settle(&program, &format!("{}-", cleanup.instance), 1).await;
    assert_eq!(left.len(), 1, "finished model VM was not removed: {left:?}");

    rig.stop().await;
    assert!(
        ids(&program, &format!("{}-", cleanup.instance)).is_empty(),
        "remaining VMs: {:?}",
        ids(&program, &format!("{}-", cleanup.instance))
    );
    drop(cleanup);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejected_activation_starts_no_vm_at_all() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let Some((program, image)) = environment() else {
        return;
    };
    let _serial = serial().await;
    let pid = std::process::id();
    let cleanup = Cleanup {
        program: program.clone(),
        instance: format!("botvm-f{pid}"),
        network: format!("botvm-canary-{pid}-fail"),
    };
    let mut rig = Rig::start_vm(
        schema,
        PROFILE,
        &cli_gateway(),
        &[Role::Gateway, Role::Worker],
        Vm {
            image,
            network: cleanup.network.clone(),
            instance: cleanup.instance.clone(),
            relay_bind: None,
        },
    )
    .await;
    // Refused activation without `--activate` must stop before launching any anchor VM.
    let error = rig
        .fail_to_start(
            &PROFILE.replace("timezone: UTC", "timezone: Asia/Seoul"),
            &[Role::Gateway, Role::Worker],
        )
        .await;
    assert!(error.to_ascii_lowercase().contains("revision"), "{error}");
    assert!(
        ids(&program, &format!("{}-", cleanup.instance)).is_empty(),
        "remaining VMs: {:?}",
        ids(&program, &format!("{}-", cleanup.instance))
    );
    drop(cleanup);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_address_that_is_not_the_anchors_gateway_stops_the_start_after_the_anchor_is_up_and_leaves_no_vm()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let Some((program, image)) = environment() else {
        return;
    };
    let _serial = serial().await;
    let pid = std::process::id();
    let cleanup = Cleanup {
        program: program.clone(),
        instance: format!("botvm-r{pid}"),
        network: format!("botvm-canary-{pid}-relay"),
    };
    // Mismatched `relay_bind` stops after launching the anchor VM and must clean it up before returning.
    let mut rig = Rig::launch_vm(
        schema,
        PROFILE,
        &cli_gateway(),
        &[Role::Gateway, Role::Worker],
        Vm {
            image,
            network: cleanup.network.clone(),
            instance: cleanup.instance.clone(),
            relay_bind: Some("10.255.255.1:0".into()),
        },
    )
    .await;
    let error = rig.failure().await;
    assert!(
        error.contains("relay_bind") && error.contains("10.255.255.1"),
        "{error}"
    );
    assert_eq!(
        rig.schema.active(),
        "",
        "active revision must not be published"
    );
    assert!(
        ids(&program, &format!("{}-", cleanup.instance)).is_empty(),
        "remaining VMs: {:?}",
        ids(&program, &format!("{}-", cleanup.instance))
    );
    drop(cleanup);
}
