//! Apple `container` adapter unit tests using a fake Python `container` CLI script.

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

    fn state(&self, value: Value) {
        let temp = self.dir.join(format!("state.json.{}", std::process::id()));
        std::fs::write(&temp, value.to_string()).unwrap();
        std::fs::rename(temp, self.dir.join("state.json")).unwrap();
    }

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

    fn config(&self) -> ContainerConfig {
        let mut config = ContainerConfig::new("alpine:3.20", "botvm-net", &self.root)
            .with_lock_dir(self.dir.join("locks"))
            .unfirewalled();
        config.program = self.dir.join("container").to_str().unwrap().to_owned();
        config
    }

    fn lock_file(&self, instance: &str) -> PathBuf {
        lock::lock_path(&self.dir.join("locks"), instance)
    }

    fn locks(&self) -> PathBuf {
        let dir = self.dir.join("locks");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        dir
    }

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

async fn up(fake: &Fake) -> ContainerSandbox {
    start(fake, fake.config())
        .await
        .relay(8787)
        .await
        .expect("relay")
}

async fn start(fake: &Fake, config: ContainerConfig) -> Anchored {
    ContainerSandbox::start_probed(
        config,
        address_while_anchored(fake),
        |_| true,
        Duration::from_secs(5),
    )
    .await
    .expect("start")
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
    let mut open = fake.config();
    open.unfirewalled = false;
    assert_eq!(
        ContainerSandbox::start(open).await.err(),
        Some(StartError("firewall_required"))
    );
    assert!(
        fake.calls().is_empty(),
        "container CLI must not be invoked: {:?}",
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
        "existing network must not be recreated"
    );
    let run = fake.argv("run").pop().unwrap();
    assert!(run.contains(&PINNED.to_owned()), "{run:?}");
    anchored.abort().await;
    fake.set(json!({"networks": {}}));
    start(&fake, fake.config()).await.abort().await;
    let created = fake
        .argv("network")
        .into_iter()
        .find(|a| a[1] == "create")
        .expect("network created");
    assert!(created.contains(&"--internal".to_owned()), "{created:?}");
    assert!(
        created
            .windows(2)
            .any(|w| w == ["--label", "bot.managed=1"]),
        "{created:?}"
    );
    assert_eq!(created.last().unwrap(), "botvm-net");
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
    assert!(fake.argv("image").iter().all(|a| a[1] == "inspect"));
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
    fake.set(json!({"lines": ["{\"a\":1}", "second line"], "exit": 3}));
    let sandbox = up(&fake).await;
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    assert_eq!(process.next_line().await.as_deref(), Some("{\"a\":1}"));
    assert_eq!(process.next_line().await.as_deref(), Some("second line"));
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
        "token must not appear in argv: {argv}"
    );
    assert_eq!(
        run["env"]["ANTHROPIC_AUTH_TOKEN"], "sess-token-secret-1",
        "token must be passed via client env"
    );
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
            // Variables injected automatically by macOS Xcode CLT `/usr/bin/python3` shim.
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
    fake.set(json!({"lines": [], "big_line": 0, "hang": true, "killed": []}));
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    process.kill().await;
    assert_eq!(fake.read()["killed"].as_array().unwrap().len(), 1);
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
        "dropped container was not killed: {:?}",
        fake.read()["killed"]
    );
}

#[tokio::test]
async fn every_spawn_rechecks_the_network_and_the_pinned_image_and_refuses_when_unsure() {
    let fake = Fake::new("recheck");
    let sandbox = up(&fake).await;
    sandbox.spawn(fake.spec()).await.expect("initial spawn");
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
    fake.set(json!({"networks": {"botvm-net": {"mode": "nat", "gateway": "192.168.99.1"}}}));
    refused("nat").await;
    fake.set(json!({"networks": {"botvm-net": {"mode": "hostOnly", "gateway": "192.168.99.7"}}}));
    refused("gateway").await;
    fake.set(json!({"networks": {}}));
    refused("no network").await;
    fake.set(good.clone());
    fake.set(json!({"images": {}}));
    refused("image removed").await;
    fake.set(json!({"images": {"alpine:3.20": "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}}));
    refused("image moved").await;
    fake.set(json!({"images": {"alpine:3.20": DIGEST}}));
    sandbox
        .spawn(fake.spec())
        .await
        .expect("spawn resumes once verified");
    sandbox.shutdown().await;
}

#[tokio::test]
async fn per_program_images_are_pinned_selected_per_cli_and_fail_closed_when_missing() {
    const CODEX_DIGEST: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const CODEX_PINNED: &str =
        "codex-img@sha256:1111111111111111111111111111111111111111111111111111111111111111";

    let fake = Fake::new("multi-image");
    fake.set(json!({
        "images": {
            "alpine:3.20": DIGEST,
            "codex-img:latest": CODEX_DIGEST,
        },
        "lines": ["ok"],
    }));
    let config = fake
        .firewalled()
        .with_image_for("codex", "codex-img:latest");
    let sandbox = start(&fake, config).await.relay(8787).await.unwrap();

    // Codex spawns with its own digest-pinned image.
    let mut codex_spec = fake.spec();
    codex_spec.program = "codex".into();
    codex_spec.args = vec![
        "exec".into(),
        "-c".into(),
        "model_provider=\"relay\"".into(),
        "-c".into(),
        "model_providers.relay.base_url=\"http://192.168.99.1:8787/v1\"".into(),
        "-c".into(),
        "mcp_servers.relay.url=\"http://192.168.99.1:8787/mcp\"".into(),
        "-".into(),
    ];
    codex_spec.env = vec![("HOME".into(), "/work/home".into())];
    let mut proc = sandbox.spawn(codex_spec).await.expect("codex spawn");
    assert_eq!(proc.next_line().await.as_deref(), Some("ok"));
    let _ = proc.wait().await;
    let run = fake
        .argv("run")
        .into_iter()
        .rfind(|a| a.contains(&"codex".to_owned()))
        .unwrap();
    assert!(run.contains(&CODEX_PINNED.to_owned()), "{run:?}");

    // On a codex-only host (non-empty `images` without a `claude` or `agy` entry), both fail closed.
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    let mut agy_spec = fake.spec();
    agy_spec.program = "agy".into();
    agy_spec.args = vec!["--print=".into()];
    agy_spec.env = vec![("HOME".into(), "/work/home".into())];
    assert!(matches!(
        sandbox.spawn(agy_spec).await,
        Err(Error::Unavailable)
    ));
    sandbox.shutdown().await;
}
