//! Helpers for activating a host-config-only revision change (moved upstream endpoint) and spawning a worker/gateway on it.
use super::{
    ClaudeParts, Gate, Launch, LocalSandbox, MOVED_ENDPOINT, Overrides, Rig, Role, Server,
    Settings, bot, env_for, host_config, model_key_store, refs, start_roles,
};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

impl Rig {
    /// Activates a new revision with only the upstream model endpoint changed (profile revision stays identical).
    pub async fn move_upstream(&mut self) -> Server {
        let before = self.schema.active();
        let host = self.dir.path().join("host-moved.json");
        std::fs::write(
            &host,
            host_config(&self.gateway, MOVED_ENDPOINT, &self.host_extra),
        )
        .unwrap();
        let socket = self.dir.path().join("moved").join("g.sock");
        let env = env_for(
            &self.dir.path().join("profiles"),
            &self.schema.url(),
            &socket,
            &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
        );
        let server = start_roles(&refs(&env), "gateway", &["--activate"], &socket);
        self.until("host-config activation did not publish", 30, |rig| {
            rig.schema.active() != before
        })
        .await;
        server
    }

    /// Launches a second in-process worker targeting the moved host config published by `move_upstream`.
    pub fn start_worker_on_moved_host(&self) -> MovedWorker {
        let host = self.dir.path().join("host-moved.json");
        assert!(
            host.exists(),
            "move_upstream must publish the new revision first"
        );
        let env = env_for(
            &self.dir.path().join("profiles"),
            &self.schema.url(),
            &self.socket,
            &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
        );
        let settings = Settings::read(&|name| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
        .unwrap();
        let stop = CancellationToken::new();
        let overrides = Overrides {
            claude: Some(ClaudeParts {
                launch: Launch::Ready {
                    sandbox: LocalSandbox::new(self.claude.clone(), &Gate::new()),
                    bind: "127.0.0.1:0".parse().unwrap(),
                    confined_reads: false,
                },
                upstream: self.upstream.clone(),
                workspace: self.dir.path().join("models-moved"),
                park: None,
            }),
            secrets: Some(model_key_store()),
            secret_admin: None,
            stop: Some(stop.clone()),
            slack_base: None,
            upstream: None,
            store_config: None,
        };
        let task = tokio::spawn(async move {
            bot_cli::serve::run_with(&settings, &[Role::Worker], false, overrides).await
        });
        MovedWorker {
            stop,
            task: Some(task),
        }
    }

    pub async fn chat_on_moved_gateway(
        &self,
        request_id: &str,
        message: &str,
    ) -> std::process::Output {
        let env = env_for(
            &self.dir.path().join("profiles"),
            &self.schema.url(),
            &self.dir.path().join("moved").join("g.sock"),
            &[(
                "BOT_HOST_CONFIG",
                self.dir.path().join("host-moved.json").to_str().unwrap(),
            )],
        );
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
}

pub struct MovedWorker {
    stop: CancellationToken,
    task: Option<JoinHandle<Result<(), String>>>,
}

impl MovedWorker {
    pub async fn stop(mut self) {
        self.stop.cancel();
        let task = self.task.take().expect("already stopped");
        let ended = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("moved worker did not stop")
            .unwrap();
        assert_eq!(ended, Ok(()), "moved worker exited with error");
    }
}

impl Drop for MovedWorker {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
