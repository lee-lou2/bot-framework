//! In-process `serve` harness for real `claude` CLI tests backed by FakeUpstream and a synthetic local sandbox.
//! Zero real model calls occur: the CLI points `ANTHROPIC_BASE_URL` at the loopback relay fronted by FakeUpstream.
//! Production uses Apple VM isolation only; the local sandbox is injected strictly via test-only `Overrides`.
#![allow(dead_code)]

use crate::process_support::{BOT, Schema, Server, bot, env_for, my_uid, refs, start_roles, text};
use bot_broker::testing::{End, FakeSecrets, FakeUpstream};
use bot_cli::{
    cli::Role,
    serve::{
        Overrides,
        claude::{ClaudeParts, HostPark, Launch},
    },
    settings::Settings,
};
use bot_core::port::SecretStore;
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{process::Command, task::JoinHandle};
use tokio_util::sync::CancellationToken;

/// Locates the `claude` binary, skipping unless `BOT_CLAUDE_CANARY=required`.
pub fn claude_bin() -> Option<PathBuf> {
    let found = std::env::var_os("BOT_CLAUDE_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PATH")?
                .to_str()?
                .split(':')
                .map(|dir| PathBuf::from(dir).join("claude"))
                .find(|path| path.is_file())
        });
    if found.is_none() {
        assert!(
            std::env::var("BOT_CLAUDE_CANARY").as_deref() != Ok("required"),
            "claude CLI is required (BOT_CLAUDE_BIN)"
        );
        eprintln!("skipped: claude CLI not found");
    }
    found
}

mod api;
mod card;
mod modes;
mod moved;
mod restart;
mod route;
mod sandbox;
mod sse;

#[allow(unused_imports)]
pub use api::*;
#[allow(unused_imports)]
pub use card::*;
#[allow(unused_imports)]
pub use modes::{Vm, slack_key_store};
#[allow(unused_imports)]
pub use moved::MovedWorker;
#[allow(unused_imports)]
pub use route::*;
pub use sandbox::{Gate, LocalSandbox};
#[allow(unused_imports)]
pub use sse::*;

/// Runs in-process `serve` for synthetic profile `personal` with FakeUpstream and a test sandbox; stops on drop.
pub struct Rig {
    pub upstream: Arc<FakeUpstream>,
    pub park: Arc<HostPark>,
    /// Barrier invoked before spawning the model CLI so tests can pause between lease and model request.
    pub gate: Gate,
    pub socket: PathBuf,
    pub env: Vec<(String, String)>,
    gateway: String,
    host_extra: String,
    claude: PathBuf,
    secrets: Arc<dyn SecretStore>,
    slack_base: Option<String>,
    launch: Option<Launch>,
    store_config: Option<bot_store::Config>,
    stop: CancellationToken,
    task: Option<JoinHandle<Result<(), String>>>,
    pub dir: tempfile::TempDir,
    /// Dropped last so the schema is removed after the server stops.
    pub schema: Schema,
}

pub const SCOPES: &str = "[run, schedule.create, schedule.list, schedule.cancel]";
const ENDPOINT: &str = "https://api.main.example";
const MOVED_ENDPOINT: &str = "https://api.moved.example";

fn host_config(gateway: &str, endpoint: &str, extra: &str) -> String {
    format!(
        r#"{{"version": 1,
            "models": {{"personal": ["assistant"]}},
            "providers": {{"assistant": {{"kind": "claude-cli", "model": "model-large",
                "endpoint": "{endpoint}", "secret": "model-key"}}}},
            "gateway": {{{gateway}}}{extra}}}"#
    )
}

pub fn model_key_store() -> Arc<FakeSecrets> {
    Arc::new(FakeSecrets::new(&[(
        "personal",
        "model-key",
        "provider-key-for-personal",
    )]))
}

impl Rig {
    pub async fn start(
        schema: Schema,
        claude: PathBuf,
        profile_yaml: &str,
        extra: &[(&str, &str)],
        gateway: &str,
        roles: &[Role],
    ) -> Rig {
        Self::start_with(
            schema,
            claude,
            profile_yaml,
            extra,
            gateway,
            roles,
            model_key_store(),
        )
        .await
    }

    pub async fn start_hosted(
        schema: Schema,
        claude: PathBuf,
        profile_yaml: &str,
        extra: &[(&str, &str)],
        gateway: &str,
        host_extra: &str,
        roles: &[Role],
    ) -> Rig {
        let mut rig = Self::launch(
            schema,
            claude,
            profile_yaml,
            extra,
            gateway,
            roles,
            model_key_store(),
            host_extra,
            None,
        )
        .await;
        rig.until("serve did not publish an active revision", 30, |rig| {
            !rig.schema.active().is_empty()
        })
        .await;
        rig
    }

    pub async fn start_with(
        schema: Schema,
        claude: PathBuf,
        profile_yaml: &str,
        extra: &[(&str, &str)],
        gateway: &str,
        roles: &[Role],
        secrets: Arc<dyn SecretStore>,
    ) -> Rig {
        let mut rig = Self::launch(
            schema,
            claude,
            profile_yaml,
            extra,
            gateway,
            roles,
            secrets,
            "",
            None,
        )
        .await;
        rig.until("serve did not publish an active revision", 30, |rig| {
            !rig.schema.active().is_empty()
        })
        .await;
        rig
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn launch(
        schema: Schema,
        claude: PathBuf,
        profile_yaml: &str,
        extra: &[(&str, &str)],
        gateway: &str,
        roles: &[Role],
        secrets: Arc<dyn SecretStore>,
        host_extra: &str,
        slack_base: Option<&str>,
    ) -> Rig {
        Self::launch_inner(
            schema,
            claude,
            profile_yaml,
            extra,
            gateway,
            roles,
            secrets,
            host_extra,
            slack_base,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn launch_inner(
        schema: Schema,
        claude: PathBuf,
        profile_yaml: &str,
        extra: &[(&str, &str)],
        gateway: &str,
        roles: &[Role],
        secrets: Arc<dyn SecretStore>,
        host_extra: &str,
        slack_base: Option<&str>,
        vm: Option<Vm>,
        store_config: Option<bot_store::Config>,
    ) -> Rig {
        let dir = tempfile::Builder::new()
            .prefix("c")
            .tempdir_in("/tmp")
            .unwrap();
        let profiles = dir.path().join("profiles");
        std::fs::create_dir_all(profiles.join("personal/agents/main")).unwrap();
        std::fs::write(profiles.join("personal/profile.yaml"), profile_yaml).unwrap();
        std::fs::write(
            profiles.join("personal/agents/main/AGENT.md"),
            "Handle the user's request and use schedule tools if follow-up is needed.",
        )
        .unwrap();
        for (path, body) in extra {
            let file = profiles.join("personal").join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, body).unwrap();
        }
        // In VM mode, configure `sandbox.apple` so the worker builds production adapter wiring with only upstream faked.
        let host_extra = match &vm {
            Some(vm) => format!(
                r#"{host_extra}, "sandbox": {{"apple": {{"image": "{}", "network": "{}", "instance": "{}", "work_root": "{}"{}}}}}"#,
                vm.image,
                vm.network,
                vm.instance,
                dir.path().join("models").display(),
                vm.relay_bind
                    .as_ref()
                    .map(|bind| format!(r#", "relay_bind": "{bind}""#))
                    .unwrap_or_default()
            ),
            None => host_extra.to_owned(),
        };
        let host_extra = host_extra.as_str();
        let host = dir.path().join("host.json");
        std::fs::write(&host, host_config(gateway, ENDPOINT, host_extra)).unwrap();
        let socket = dir.path().join("run").join("g.sock");
        let installs = dir.path().join("installs");
        let env = env_for(
            &profiles,
            &schema.url(),
            &socket,
            &[
                ("BOT_HOST_CONFIG", host.to_str().unwrap()),
                ("BOT_INSTALLS", installs.to_str().unwrap()),
            ],
        );
        let settings = Settings::read(&|name| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
        .unwrap();
        let upstream = Arc::new(FakeUpstream::new());
        let park = Arc::new(HostPark::default());
        let gate = Gate::new();
        let stop = CancellationToken::new();
        let launch = vm.is_none().then(|| Launch::Ready {
            sandbox: LocalSandbox::new(claude.clone(), &gate),
            bind: "127.0.0.1:0".parse().unwrap(),
            confined_reads: false,
        });
        let overrides = Overrides {
            claude: launch.clone().map(|launch| ClaudeParts {
                launch,
                upstream: upstream.clone(),
                workspace: dir.path().join("models"),
                park: Some(park.clone()),
            }),
            upstream: launch.is_none().then(|| upstream.clone() as _),
            store_config: store_config.clone(),
            secrets: Some(secrets.clone()),
            secret_admin: None,
            stop: Some(stop.clone()),
            slack_base: slack_base.map(str::to_owned),
        };
        let roles = roles.to_vec();
        let task = tokio::spawn(async move {
            bot_cli::serve::run_with(&settings, &roles, false, overrides).await
        });
        Rig {
            upstream,
            park,
            gate,
            socket,
            env,
            gateway: gateway.to_owned(),
            host_extra: host_extra.to_owned(),
            claude,
            secrets,
            slack_base: slack_base.map(str::to_owned),
            launch,
            store_config,
            stop,
            task: Some(task),
            dir,
            schema,
        }
    }

    pub async fn until(&mut self, what: &str, secs: u64, mut ready: impl FnMut(&Rig) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            if ready(self) {
                return;
            }
            if self.task.as_ref().is_some_and(JoinHandle::is_finished) {
                let ended = self.task.take().unwrap().await;
                panic!("serve exited unexpectedly: {ended:?} ({what})");
            }
            assert!(tokio::time::Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub fn script(&self, chunks: Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        self.upstream.respond_chunks(200, &refs, End::Eof);
    }

    pub fn script_slowly(&self, chunks: Vec<Vec<u8>>, gap: Duration) {
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        self.upstream.respond_slowly(200, &refs, gap, End::Eof);
    }

    pub async fn chat(&self, request_id: &str, message: &str) -> std::process::Output {
        let env = self.env.clone();
        let (request_id, message) = (request_id.to_owned(), message.to_owned());
        tokio::task::spawn_blocking(move || {
            bot(
                &refs(&env),
                &["chat", "--request-id", &request_id, &message],
            )
        })
        .await
        .unwrap()
    }

    pub fn chat_in_background(&self, request_id: &str, message: &str) -> tokio::process::Child {
        Command::new(BOT)
            .args(["chat", "--request-id", request_id, message])
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn bot chat")
    }

    pub fn query(&self, sql: &str) -> String {
        self.schema.query(sql)
    }

    pub async fn stop(mut self) {
        self.stop.cancel();
        let task = self.task.take().expect("serve already exited");
        let ended = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("serve did not stop")
            .unwrap();
        assert_eq!(ended, Ok(()), "serve exited with error");
    }

    pub async fn failure(&mut self) -> String {
        let task = self.task.take().expect("serve already exited");
        let ended = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("serve did not finish")
            .unwrap();
        ended.expect_err("serve unexpectedly succeeded")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub fn cli_gateway() -> String {
    format!(
        r#""cli": {{"accounts": {{"owner": {{"uid": {}}}}}}}"#,
        my_uid()
    )
}

pub fn said(output: &std::process::Output) -> String {
    text(output)
}
