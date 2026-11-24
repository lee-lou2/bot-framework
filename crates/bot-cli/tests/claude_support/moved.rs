//! 호스트 설정만 바뀐 새 판본(`--activate`)과 그 worker·gateway를 다루는 장치: 모델 상류 목적지가 옮겨 간 뒤 구판·신판이 어떻게 다루는지 본다.
use super::{
    ClaudeParts, Gate, Launch, LocalSandbox, MOVED_ENDPOINT, Overrides, Rig, Role, Server,
    Settings, bot, env_for, host_config, model_key_store, refs, start_roles,
};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

impl Rig {
    /// 같은 프로필 폴더로 **모델 상류 목적지만** 바꾼 호스트 설정을 `--activate`로 게시한다(gateway 역할만 띄운 두 번째 프로세스).
    /// 프로필 판본은 그대로이고 활성화 판본만 바뀐다. 바뀔 때까지 기다리고, 돌려준 프로세스를 버리면 죽는다.
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
        self.until(
            "호스트 설정을 바꾼 활성화가 게시되지 않았어요",
            30,
            |rig| rig.schema.active() != before,
        )
        .await;
        server
    }

    /// `move_upstream`이 게시한 새 판본(호스트 설정만 바뀐 같은 프로필)의 worker를 같은 프로세스 안에서 하나 더 띄운다. 모델 상류 목적지는 옮긴
    /// 쪽이고 가짜 상류·자격 보관소는 그대로다. 돌려준 값을 버리면 멈춘다.
    pub fn start_worker_on_moved_host(&self) -> MovedWorker {
        let host = self.dir.path().join("host-moved.json");
        assert!(
            host.exists(),
            "먼저 move_upstream으로 새 판본을 게시해야 해요"
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

    /// `move_upstream`이 띄운 새 판본 gateway에 붙는 `bot chat`(같은 요청 ID로 다시 붙으면 기존 결과를 받는다)
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

/// 새 판본에서 도는 worker(같은 프로세스 안의 두 번째 `serve`). 버리면 멈춘다.
pub struct MovedWorker {
    stop: CancellationToken,
    task: Option<JoinHandle<Result<(), String>>>,
}

impl MovedWorker {
    /// 멈추고 오류 없이 끝났는지 본다.
    pub async fn stop(mut self) {
        self.stop.cancel();
        let task = self.task.take().expect("이미 끝났어요");
        let ended = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("새 worker가 멈추지 않았어요")
            .unwrap();
        assert_eq!(ended, Ok(()), "새 worker가 오류로 끝났어요");
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
