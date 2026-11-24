//! 실제 Apple `container` VM과 운영 모델 이미지(`BOT_MODEL_IMAGE`)로 `serve`의 운영 조립을 확인한다 — 합성 샌드박스가 아니라 어댑터 그대로:
//! 닻 VM → 그 게이트웨이 주소에 프런트 열기 → 릴레이 알림 → 모델 VM의 실제 `claude`가 프런트로만 모델 응답을 받는다(상류는 가짜라 모델 호출 0).
//! serve를 멈추면 닻과 이번 실행의 VM이 모두 치워진다.
//!
//! 실행: `BOT_CONTAINER_CANARY=required BOT_MODEL_IMAGE=<이름@sha256:…> BOT_TEST_DATABASE_URL=… cargo test -p bot-cli --test vm_serve -- --nocapture`
//! (`container` 서비스·로컬 이미지가 없으면 건너뛴다. `BOT_CONTAINER_CANARY=required`이면 실패한다.) 만들고 지우는 것은 이 시험의
//! `botvm-s<pid>-*` 컨테이너와 `botvm-canary-<pid>-serve` 망뿐이다.
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

/// 쓸 수 있는 환경이면 `(container 경로, 모델 이미지 참조)`. 아니면 건너뛴다(`BOT_CONTAINER_CANARY=required`이면 실패).
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
            "BOT_CONTAINER_CANARY=required: container CLI·서비스와 BOT_MODEL_IMAGE=<이름@sha256:…> 로컬 이미지가 필요하다(images/model/build.sh)"
        );
        eprintln!("skipped: container CLI·서비스 또는 BOT_MODEL_IMAGE 로컬 이미지가 없어요");
    }
    found
}

/// 이 시험의 접두사로 보이는 컨테이너 ID들(멈춘 것 포함)
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

/// 접두사가 같은 컨테이너가 `want`개가 될 때까지(최대 30초) 기다리고 마지막으로 본 ID들을 돌려준다. 끝난 모델 VM의 `--rm` 삭제는
/// 비동기라 막 끝난 VM이 잠깐 목록에 남는다 — 바로 세면 부하가 있을 때 간헐로 실패한다(관문 실행에서 «2 ≠ 1»로 한 번 실패).
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

/// 이 기계에서 도는 실제 VM 시험(다른 역할의 시험 포함)은 한 번에 하나만 돈다: 서로의 VM 시작·망을 흔들지 않게 같은 기계 전체 잠금 파일
/// (`/tmp/bot-vm-canary.lock`, ②의 `vm::serial`과 같은 것)을 쥔다. 같은 프로세스 안에서는 시험이 차례로 돈다. 다른 시험이 15분 넘게 쥐고 있으면 잠금 없이 진행한다.
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
                .expect("VM 시험 잠금 파일");
            for _ in 0..4500 {
                if file.try_lock().is_ok() {
                    return file;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            eprintln!("warning: 다른 VM 시험이 잠금을 오래 쥐고 있어 잠금 없이 진행한다");
            file
        })
        .await
        .unwrap();
        let _ = MACHINE.set(file);
    }
    guard
}

/// 이 시험의 컨테이너와 망만 지운다(패닉해도).
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
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
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
    // worker가 서 있으면 닻 VM이 이미 돌고 있다(모델 VM은 시도마다 뜨고 끝나면 사라진다)
    assert_eq!(
        ids(&program, &format!("{}-", cleanup.instance)).len(),
        1,
        "닻 VM 하나가 돌고 있어야 해요"
    );

    // 대화: 모델 VM의 실제 claude가 프런트(닻이 지키는 게이트웨이 주소)로만 모델 응답을 받아 답한다
    rig.script(final_turn("msg_vm1", "VM 안의 모델이 답했어요"));
    let out = rig.chat("vm1", "안녕").await;
    assert!(out.status.success(), "{}", said(&out));
    assert!(
        said(&out).contains("VM 안의 모델이 답했어요"),
        "{}",
        said(&out)
    );
    assert_eq!(
        rig.upstream.requests().len(),
        1,
        "상류로는 한 번만 나갔어요"
    );
    // 모델 VM은 끝나면 사라지고 닻만 남는다
    let left = settle(&program, &format!("{}-", cleanup.instance), 1).await;
    assert_eq!(left.len(), 1, "끝난 모델 VM이 사라지지 않았어요: {left:?}");

    // serve를 멈추면 닻과 이번 실행의 VM이 모두 치워진다
    rig.stop().await;
    assert!(
        ids(&program, &format!("{}-", cleanup.instance)).is_empty(),
        "남은 VM: {:?}",
        ids(&program, &format!("{}-", cleanup.instance))
    );
    drop(cleanup);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejected_activation_starts_no_vm_at_all() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
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
    // 다른 프로필(`--activate` 없음)로 다시 시작하면 활성 판본 결정이 거절된다 — 닻 VM을 띄우기 전이라 어떤 VM도 남지 않는다
    let error = rig
        .fail_to_start(
            &PROFILE.replace("timezone: UTC", "timezone: Asia/Seoul"),
            &[Role::Gateway, Role::Worker],
        )
        .await;
    assert!(error.contains("판본"), "{error}");
    assert!(
        ids(&program, &format!("{}-", cleanup.instance)).is_empty(),
        "남은 VM: {:?}",
        ids(&program, &format!("{}-", cleanup.instance))
    );
    drop(cleanup);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_address_that_is_not_the_anchors_gateway_stops_the_start_after_the_anchor_is_up_and_leaves_no_vm()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
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
    // 같은 판본으로 처음 시작한다: 닻 VM은 뜨지만 `relay_bind`가 닻이 확인한 게이트웨이 주소가 아니라서 프런트를 열기 전에 멈춘다
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
    assert_eq!(rig.schema.active(), "", "활성 판본도 게시되지 않았어요");
    // 닻을 띄운 뒤에 멈췄으므로 치우는 것까지 끝나야 한다
    assert!(
        ids(&program, &format!("{}-", cleanup.instance)).is_empty(),
        "남은 VM: {:?}",
        ids(&program, &format!("{}-", cleanup.instance))
    );
    drop(cleanup);
}
