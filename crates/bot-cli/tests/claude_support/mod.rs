//! 실제 `claude` CLI(가짜 상류)로 예약 흐름을 확인하는 시험 도구: CLI를 호스트 프로세스로 띄우는 합성 샌드박스(VM 대신), 가짜 상류의
//! SSE 응답, 프로세스 안에서 `serve`를 돌리는 장치. 모델 호출은 0이다 — CLI의 `ANTHROPIC_BASE_URL`은 프런트(루프백)이고 프런트의 상류는 가짜다.
//! 운영 모델 샌드박스는 Apple VM뿐이므로 이 합성 샌드박스는 시험에서만 `Overrides`로 넣는다.
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

/// 실제 CLI가 없으면 건너뛴다. `BOT_CLAUDE_CANARY=required`이면 실패한다.
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
            "claude CLI가 필요하다(BOT_CLAUDE_BIN)"
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

// ---- 프로세스 안에서 serve를 돌리는 장치 ----

/// 합성 프로필 `personal`(역할 owner가 예약 스코프 셋을 가진다)과 가짜 상류·합성 샌드박스 위에서 `serve`를 돌린다. 버리면 멈춘다.
pub struct Rig {
    pub upstream: Arc<FakeUpstream>,
    /// 모델 프런트가 대기(승인·보류·자식)를 알린 곳(실제 호스트 구현)
    pub park: Arc<HostPark>,
    /// 모델 CLI를 띄우기 전에 서는 문(시도는 집었지만 모델 요청은 아직 안 나간 순간을 만든다)
    pub gate: Gate,
    pub socket: PathBuf,
    pub env: Vec<(String, String)>,
    /// 호스트 `gateway` 절과 더 붙인 최상위 절(다시 만드는 호스트 설정에 쓴다)
    gateway: String,
    host_extra: String,
    /// 모델 CLI 실행 파일(새 worker도 같은 것을 쓴다)
    claude: PathBuf,
    /// 자격 보관소와 가짜 Slack 주소(다시 활성화해 띄우는 serve도 같은 것을 쓴다)
    secrets: Arc<dyn SecretStore>,
    slack_base: Option<String>,
    /// 모델 샌드박스를 얻는 방법(다시 띄우는 serve도 같은 것을 쓴다)
    launch: Option<Launch>,
    /// 저장소 설정(시도 한도 같은 값을 바꿔 본 시험에서만 있다)
    store_config: Option<bot_store::Config>,
    stop: CancellationToken,
    task: Option<JoinHandle<Result<(), String>>>,
    pub dir: tempfile::TempDir,
    /// 마지막에 정리한다(서버가 멈춘 뒤)
    pub schema: Schema,
}

pub const SCOPES: &str = "[run, schedule.create, schedule.list, schedule.cancel]";
const ENDPOINT: &str = "https://api.main.example";
const MOVED_ENDPOINT: &str = "https://api.moved.example";

/// 합성 호스트 설정: 모델 별칭 `assistant`는 `claude-cli`(상류 `endpoint`, 서비스 키 이름 `model-key`). `extra`는 더 붙일 최상위 절(`, "servers": …` 꼴)
fn host_config(gateway: &str, endpoint: &str, extra: &str) -> String {
    format!(
        r#"{{"version": 1,
            "models": {{"personal": ["assistant"]}},
            "providers": {{"assistant": {{"kind": "claude-cli", "model": "model-large",
                "endpoint": "{endpoint}", "secret": "model-key"}}}},
            "gateway": {{{gateway}}}{extra}}}"#
    )
}

/// 시험이 쓰는 기본 보관소: 프로필 personal의 모델 서비스 키 하나
pub fn model_key_store() -> Arc<FakeSecrets> {
    Arc::new(FakeSecrets::new(&[(
        "personal",
        "model-key",
        "provider-key-for-personal",
    )]))
}

impl Rig {
    /// `profile_yaml`은 `personal/profile.yaml`, `extra`는 프로필 폴더 안의 더 만들 파일들(`personal/` 밑 상대 경로, 내용),
    /// `gateway`는 호스트 `gateway` 절의 JSON 본문(중괄호 안)이다. 활성 판본이 게시될 때까지 기다린다.
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

    /// `start`와 같되 호스트 설정에 최상위 절을 더 붙인다(`host_extra`는 `, "servers": …` 꼴).
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
        rig.until(
            "serve가 활성 판본을 게시하지 않았어요",
            30,
            |rig| !rig.schema.active().is_empty(),
        )
        .await;
        rig
    }

    /// `start`와 같되 자격 보관소를 정한다.
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
        rig.until(
            "serve가 활성 판본을 게시하지 않았어요",
            30,
            |rig| !rig.schema.active().is_empty(),
        )
        .await;
        rig
    }

    /// serve를 띄우기만 한다(시작이 실패하는 경우를 볼 때는 `failure`).
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
            "요청받은 일을 하고 필요하면 예약 도구로 나중 일을 예약한다.",
        )
        .unwrap();
        for (path, body) in extra {
            let file = profiles.join("personal").join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, body).unwrap();
        }
        // 운영 모양(VM)이면 호스트 설정에 `sandbox.apple`을 적는다: 어댑터 조각은 worker가 이 설정에서 만든다(작업 폴더는 serve가 0700으로 만든다)
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
        // 도구 설치본은 시험 폴더에 둔다(사용자의 홈을 건드리지 않는다)
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
        // 합성 샌드박스(시험) 또는 호스트 설정에서 worker가 만드는 운영 조각(`claude: None`) — 운영 조각이면 상류만 가짜로 바꾼다
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

    /// 조건이 될 때까지 기다린다. 그 사이 serve가 끝나 버리면 그 오류로 실패한다.
    pub async fn until(&mut self, what: &str, secs: u64, mut ready: impl FnMut(&Rig) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            if ready(self) {
                return;
            }
            if self.task.as_ref().is_some_and(JoinHandle::is_finished) {
                let ended = self.task.take().unwrap().await;
                panic!("serve가 끝났어요: {ended:?} ({what})");
            }
            assert!(tokio::time::Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// 다음 모델 요청에 이 응답을 준다.
    pub fn script(&self, chunks: Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        self.upstream.respond_chunks(200, &refs, End::Eof);
    }

    /// 다음 모델 요청에 이 응답을 조각마다 `gap`씩 늦춰 흘려 준다(스트림이 열려 있는 동안 일어나는 일을 보려고).
    pub fn script_slowly(&self, chunks: Vec<Vec<u8>>, gap: Duration) {
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        self.upstream.respond_slowly(200, &refs, gap, End::Eof);
    }

    /// `bot chat`(실제 CLI 어댑터 경로)으로 한 마디 한다.
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

    /// `bot chat`을 기다리지 않고 띄운다. 돌려준 값을 버리면 죽인다.
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
            .expect("bot chat 시작")
    }

    /// 조회 한 값(`{s}`는 스키마 이름)
    pub fn query(&self, sql: &str) -> String {
        self.schema.query(sql)
    }

    /// serve를 멈추고 오류 없이 끝났는지 본다.
    pub async fn stop(mut self) {
        self.stop.cancel();
        let task = self.task.take().expect("serve가 이미 끝났어요");
        let ended = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("serve가 멈추지 않았어요")
            .unwrap();
        assert_eq!(ended, Ok(()), "serve가 오류로 끝났어요");
    }

    /// 시작이 실패했어야 하는 serve가 끝나기를 기다려 그 오류를 돌려준다. 시작에 성공했으면 실패한다.
    pub async fn failure(&mut self) -> String {
        let task = self.task.take().expect("serve가 이미 끝났어요");
        let ended = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("serve가 끝나지 않았어요")
            .unwrap();
        ended.expect_err("serve가 시작에 성공했어요")
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

/// 합성 호스트의 `gateway` 절(CLI 계정 `owner` → 이 OS 사용자)
pub fn cli_gateway() -> String {
    format!(
        r#""cli": {{"accounts": {{"owner": {{"uid": {}}}}}}}"#,
        my_uid()
    )
}

/// 출력 글(표준출력+표준오류)
pub fn said(output: &std::process::Output) -> String {
    text(output)
}
