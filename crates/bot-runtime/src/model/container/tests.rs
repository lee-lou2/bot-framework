//! Apple `container` 어댑터: 가짜 `container` 실행 파일(파이썬 스크립트)로 명령줄·점검·시작·실행·종료를 확인한다. 진짜 CLI는 `tests/container_*.rs`가 본다.

use super::*;
use args::{Entry, Plan, anchor_owner, build, build_anchor, name_of};
use bot_core::{
    id::*,
    sandbox::{Mount, Sandbox},
};
use script::FAKE;
use serde_json::{Value, json};
use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
};

mod anchor;
mod cancel;
mod command;
mod ownership;
mod reap;
mod script;
mod subnet;

const DIGEST: &str = "sha256:abababababababababababababababababababababababababababababababab";
const PINNED: &str =
    "alpine@sha256:abababababababababababababababababababababababababababababababab";

struct Fake {
    dir: PathBuf,
    root: PathBuf,
}

impl Fake {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bot-container-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("mounts/input")).unwrap();
        std::fs::create_dir_all(dir.join("mounts/work")).unwrap();
        let script = dir.join("container");
        std::fs::write(&script, FAKE).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fake = Self {
            root: dir.join("mounts"),
            dir,
        };
        fake.state(json!({
            "networks": {"botvm-net": {"mode": "hostOnly", "gateway": "192.168.99.1"}},
            "images": {"alpine:3.20": DIGEST},
            "containers": [], "killed": [], "deleted": [],
            "watch": ["ANTHROPIC_AUTH_TOKEN"],
        }));
        fake
    }

    /// 상태를 통째로 바꿔 놓는다(가짜 `container`가 읽는 도중에 반쯤 쓰인 파일을 보지 않게 임시 파일을 거친다).
    fn state(&self, value: Value) {
        let temp = self.dir.join(format!("state.json.{}", std::process::id()));
        std::fs::write(&temp, value.to_string()).unwrap();
        std::fs::rename(temp, self.dir.join("state.json")).unwrap();
    }

    /// 상태 일부를 바꾼다. 가짜 `container`의 읽고 고쳐 쓰기와 겹치지 않게 같은 잠금을 쥐고 한다.
    fn set(&self, patch: Value) {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("state.lock"))
            .unwrap();
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
        let mut state = self.read();
        for (key, value) in patch.as_object().unwrap() {
            state[key] = value.clone();
        }
        self.state(state);
    }

    fn read(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.dir.join("state.json")).unwrap()).unwrap()
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn argv(&self, first: &str) -> Vec<Vec<String>> {
        self.calls()
            .iter()
            .filter(|c| c["argv"][0] == first)
            .map(|c| {
                c["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_owned())
                    .collect()
            })
            .collect()
    }

    /// 방화벽 진입 없이(시험 전용) 시작하는 설정.
    fn config(&self) -> ContainerConfig {
        let mut config = ContainerConfig::new("alpine:3.20", "botvm-net", &self.root)
            .with_lock_dir(self.dir.join("locks"))
            .unfirewalled();
        config.program = self.dir.join("container").to_str().unwrap().to_owned();
        config
    }

    /// 이 시험의 인스턴스 잠금 파일 경로.
    fn lock_file(&self, instance: &str) -> PathBuf {
        lock::lock_path(&self.dir.join("locks"), instance)
    }

    /// 잠금 폴더를 미리 만든다(0700): 시작이 만들기 전에 잠금 파일을 쥐어 보려는 시험용.
    fn locks(&self) -> PathBuf {
        let dir = self.dir.join("locks");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        dir
    }

    /// 이미지의 신뢰 진입을 쓰는(운영 모양) 설정.
    fn firewalled(&self) -> ContainerConfig {
        let mut config = self.config();
        config.unfirewalled = false;
        config.entry = Some(Entry {
            anchor: "/usr/local/bin/bot-wait".into(),
            anchor_user: (10002, 10002),
            model_caps: vec!["NET_ADMIN".into(), "SETPCAP".into()],
        });
        config
    }

    fn spec(&self) -> ModelSpec {
        ModelSpec {
            attempt: Attempt {
                profile: ProfileId("personal".into()),
                run: RunId("run-1".into()),
                claim: Some(ClaimEpoch(3)),
                generation: Generation(2),
            },
            program: "claude".into(),
            args: vec!["-p".into(), "--model".into(), "m".into()],
            env: vec![
                (
                    "ANTHROPIC_BASE_URL".into(),
                    "http://192.168.99.1:8787".into(),
                ),
                ("ANTHROPIC_AUTH_TOKEN".into(), "sess-token-secret-1".into()),
                ("HOME".into(), "/work/home".into()),
            ],
            cwd: "/bot".into(),
            readonly_mounts: vec![Mount {
                host: self.root.join("input"),
                guest: "/bot".into(),
            }],
            scratch: Some(Mount {
                host: self.root.join("work"),
                guest: "/work".into(),
            }),
            stdin: Some(b"hello model".to_vec()),
            timeout: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(10),
        }
    }

    /// 목록에 올라 있는 VM 하나(라벨은 가진 것만).
    fn vm(&self, id: &str, labels: Value) -> Value {
        json!({"configuration": {"id": id, "labels": labels}, "status": {"state": "running"}})
    }

    fn ids(&self) -> Vec<String> {
        self.read()["containers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["configuration"]["id"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn relay() -> SocketAddr {
    "192.168.99.1:8787".parse().unwrap()
}

fn plan(relay: Option<SocketAddr>) -> Plan<'static> {
    Plan {
        image: PINNED,
        incarnation: "inc1",
        serial: 7,
        relay,
    }
}

/// 호스트에 게이트웨이가 생기는 때를 흉내 낸다: 가짜 CLI가 닻 VM을 목록에 올린 뒤에야 주소가 있다(닻이 없으면 없다).
fn address_while_anchored(fake: &Fake) -> impl Fn(Ipv4Addr) -> bool + Send + Sync + 'static {
    let state = fake.dir.join("state.json");
    move |_| {
        std::fs::read(&state)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| {
                v["containers"]
                    .as_array()
                    .map(|c| c.iter().any(|c| c["anchor"] == true))
            })
            .unwrap_or(false)
    }
}

/// 시작해서 릴레이까지 연 어댑터(가짜 CLI).
async fn up(fake: &Fake) -> ContainerSandbox {
    start(fake, fake.config())
        .await
        .relay(8787)
        .await
        .expect("릴레이")
}

async fn start(fake: &Fake, config: ContainerConfig) -> Anchored {
    ContainerSandbox::start_probed(
        config,
        address_while_anchored(fake),
        |_| true,
        Duration::from_secs(5),
    )
    .await
    .expect("시작")
}

#[tokio::test]
async fn start_refuses_an_invalid_config_or_a_missing_firewall_before_any_command_runs() {
    let fake = Fake::new("start-config");
    let mut bad = fake.config();
    bad.prefix = "a b".into();
    assert_eq!(
        ContainerSandbox::start(bad).await.err(),
        Some(StartError("config"))
    );
    // 운영은 이미지의 신뢰 init이 있어야 한다
    let mut open = fake.config();
    open.unfirewalled = false;
    assert_eq!(
        ContainerSandbox::start(open).await.err(),
        Some(StartError("firewall_required"))
    );
    assert!(
        fake.calls().is_empty(),
        "container를 한 번도 부르지 않았다: {:?}",
        fake.calls()
    );
    assert!(!fake.lock_file("botvm").exists());
}

#[tokio::test]
async fn start_requires_a_host_only_network_and_a_local_image_and_pins_the_image() {
    let fake = Fake::new("start-checks");
    let anchored = start(&fake, fake.config()).await;
    assert_eq!(anchored.address(), Ipv4Addr::new(192, 168, 99, 1));
    assert!(
        fake.argv("network").iter().all(|a| a[1] != "create"),
        "있으면 만들지 않는다"
    );
    // 태그가 아니라 고정한 다이제스트로 띄운다
    let run = fake.argv("run").pop().unwrap();
    assert!(run.contains(&PINNED.to_owned()), "{run:?}");
    anchored.abort().await;
    // 없으면 호스트 전용(`--internal`)에 관리 라벨을 붙여 만든다
    fake.set(json!({"networks": {}}));
    start(&fake, fake.config()).await.abort().await;
    let created = fake
        .argv("network")
        .into_iter()
        .find(|a| a[1] == "create")
        .expect("만들었다");
    assert!(created.contains(&"--internal".to_owned()), "{created:?}");
    assert!(
        created
            .windows(2)
            .any(|w| w == ["--label", "bot.managed=1"]),
        "{created:?}"
    );
    assert_eq!(created.last().unwrap(), "botvm-net");
    // 호스트 전용이 아니거나 게이트웨이를 모르거나 만들 수 없거나 이미지가 없으면 띄우지 않는다
    for (state, want) in [
        (
            json!({"networks": {"botvm-net": {"mode": "nat", "gateway": "1.2.3.4"}}}),
            "network_not_internal",
        ),
        (
            json!({"networks": {"botvm-net": {"mode": "hostOnly", "gateway": ""}}}),
            "network_no_gateway",
        ),
        (
            json!({"networks": {"botvm-net": {"mode": "hostOnly", "gateway": "evil.example"}}}),
            "network_no_gateway",
        ),
        (
            json!({"networks": {}, "create_fails": true}),
            "network_create_failed",
        ),
        (
            json!({"networks": {"botvm-net": {"mode": "hostOnly", "gateway": "192.168.99.1"}}, "create_fails": false, "images": {}}),
            "image_missing",
        ),
    ] {
        fake.set(state);
        let error = ContainerSandbox::start_probed(
            fake.config(),
            |_| true,
            |_| true,
            Duration::from_secs(1),
        )
        .await
        .err();
        assert_eq!(error, Some(StartError(want)), "{want}");
    }
    // 이미지를 내려받지 않는다: `image pull`은 부르지 않는다
    assert!(fake.argv("image").iter().all(|a| a[1] == "inspect"));
    // 고정한 이미지가 비정상이면(다이제스트를 못 읽음) 시작하지 않는다
    fake.set(json!({"images": {"alpine:3.20": "not-a-digest"}}));
    let error =
        ContainerSandbox::start_probed(fake.config(), |_| true, |_| true, Duration::from_secs(1))
            .await
            .err();
    assert_eq!(error, Some(StartError("image_unpinnable")));
}

#[tokio::test]
async fn the_relay_must_be_listening_on_the_gateway_before_models_can_start() {
    let fake = Fake::new("relay-up");
    let down = ContainerSandbox::start_probed(
        fake.config(),
        address_while_anchored(&fake),
        |_| false,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(
        down.relay(8787).await.err(),
        Some(StartError("relay_unreachable"))
    );
    // 열지 못한 시작은 닻을 남기지 않고 소유권도 쥐지 않는다
    for _ in 0..100 {
        if fake.read()["killed"]
            .as_array()
            .is_some_and(|k| !k.is_empty())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!fake.read()["killed"].as_array().unwrap().is_empty());
    let zero = start(&fake, fake.config()).await;
    assert_eq!(zero.relay(0).await.err(), Some(StartError("relay_port")));
    up(&fake).await.shutdown().await;
}

#[tokio::test]
async fn a_spawned_vm_streams_lines_takes_the_request_on_stdin_and_keeps_the_secret_in_the_client_env()
 {
    let fake = Fake::new("spawn");
    fake.set(json!({"lines": ["{\"a\":1}", "두번째 줄"], "exit": 3}));
    let sandbox = up(&fake).await;
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    assert_eq!(process.next_line().await.as_deref(), Some("{\"a\":1}"));
    assert_eq!(process.next_line().await.as_deref(), Some("두번째 줄"));
    assert_eq!(process.next_line().await, None);
    let exit = process.wait().await;
    assert_eq!(exit.code, Some(3));
    assert_eq!(
        std::fs::read_to_string(fake.dir.join("stdin.txt")).unwrap(),
        "hello model"
    );
    let run = fake
        .calls()
        .into_iter()
        .rfind(|c| c["argv"][0] == "run" && c["argv"].to_string().contains("claude"))
        .unwrap();
    let argv = run["argv"].to_string();
    assert!(
        !argv.contains("sess-token-secret-1"),
        "값은 명령줄에 없다: {argv}"
    );
    assert_eq!(
        run["env"]["ANTHROPIC_AUTH_TOKEN"], "sess-token-secret-1",
        "클라이언트 환경에는 있다"
    );
    // 호스트 환경은 상속하지 않는다(필요한 몇 개만)
    let keys: Vec<String> = run["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_owned())
        .collect();
    assert!(
        keys.iter().all(|k| [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "TMPDIR",
            "LANG",
            "ANTHROPIC_AUTH_TOKEN",
            "LC_CTYPE",
            "__CF_USER_TEXT_ENCODING",
            "PWD",
            "SHLVL",
            "_",
            // macOS Xcode CLT `/usr/bin/python3` 심(shim)이 실행 시 스스로 주입하는 SDK·경로 변수
            "CPATH",
            "LIBRARY_PATH",
            "MANPATH",
            "SDKROOT"
        ]
        .contains(&k.as_str())),
        "{keys:?}"
    );
    sandbox.shutdown().await;
}

#[tokio::test]
async fn an_overlong_line_ends_the_container_and_kill_and_drop_end_it_too() {
    let fake = Fake::new("limits");
    let sandbox = up(&fake).await;
    // 줄 하나가 상한을 넘으면 읽기를 끝내고 그 컨테이너를 끝낸다
    fake.set(json!({"lines": ["ok"], "big_line": process::MAX_LINE + 10, "killed": []}));
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    assert_eq!(process.next_line().await.as_deref(), Some("ok"));
    assert_eq!(process.next_line().await, None);
    let killed = fake.read()["killed"].clone();
    assert!(
        killed
            .as_array()
            .unwrap()
            .iter()
            .any(|id| is_ours("botvm", id.as_str().unwrap())),
        "{killed}"
    );
    drop(process);
    // 끝내라고 하면 컨테이너부터 끝낸다
    fake.set(json!({"lines": [], "big_line": 0, "hang": true, "killed": []}));
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    process.kill().await;
    assert_eq!(fake.read()["killed"].as_array().unwrap().len(), 1);
    // 끝났다는 확인 없이 놓으면(취소·패닉) 최선으로 끝내라고 한다
    fake.set(json!({"killed": []}));
    let process = sandbox.spawn(fake.spec()).await.unwrap();
    drop(process);
    for _ in 0..100 {
        if !fake.read()["killed"].as_array().unwrap().is_empty() {
            sandbox.shutdown().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "놓은 컨테이너를 끝내라고 하지 않았다: {:?}",
        fake.read()["killed"]
    );
}

#[tokio::test]
async fn every_spawn_rechecks_the_network_and_the_pinned_image_and_refuses_when_unsure() {
    let fake = Fake::new("recheck");
    let sandbox = up(&fake).await;
    sandbox.spawn(fake.spec()).await.expect("처음에는 열린다");
    let refused = |what: &'static str| {
        let sandbox = &sandbox;
        let spec = fake.spec();
        async move {
            assert!(
                matches!(sandbox.spawn(spec).await, Err(Error::Unavailable)),
                "{what}"
            );
        }
    };
    let good = json!({"networks": {"botvm-net": {"mode": "hostOnly", "gateway": "192.168.99.1"}}});
    // 망이 호스트 전용이 아니게 바뀌었다·게이트웨이가 바뀌었다·망이 사라졌다·조회가 실패한다
    fake.set(json!({"networks": {"botvm-net": {"mode": "nat", "gateway": "192.168.99.1"}}}));
    refused("nat").await;
    fake.set(json!({"networks": {"botvm-net": {"mode": "hostOnly", "gateway": "192.168.99.7"}}}));
    refused("gateway").await;
    fake.set(json!({"networks": {}}));
    refused("no network").await;
    // 고정한 이미지가 로컬에서 사라졌다(태그가 다른 이미지로 옮겨 가도 다이제스트가 다르면 같은 이미지가 아니다)
    fake.set(good.clone());
    fake.set(json!({"images": {}}));
    refused("image removed").await;
    fake.set(json!({"images": {"alpine:3.20": "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}}));
    refused("image moved").await;
    fake.set(json!({"images": {"alpine:3.20": DIGEST}}));
    sandbox
        .spawn(fake.spec())
        .await
        .expect("확인되면 다시 열린다");
    sandbox.shutdown().await;
}
