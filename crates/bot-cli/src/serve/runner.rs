//! 도구 실행기(srt 샌드박스 안의 MCP 서버)와 읽기 전용 설치본. worker의 브로커와 민감 DM 접수(`PrivateGate`)가 같은 방식으로 만든다.
//!
//! 호스트에서 계산하는 값: srt 실행 파일(`sandbox.srt.binary`, 내장 데모는 PATH의 `srt`)·작업 루트(`sandbox.srt.work_root`, 기본은
//! 설치 폴더 옆 `work`)·srt가 쓰는 PATH(srt 폴더 + 시스템 기본)·도구가 읽지 못하는 곳(홈·프로필 루트·호스트 설정·비밀 파일·소켓·설치 기준).
use super::{guard::GuardedWire, watched::Watched};
use crate::{
    host::HostFile,
    install::{self, Installs},
    settings::Settings,
};
use bot_broker::{
    ports::ToolRunner,
    proxy::{SystemResolver, TcpConnector},
    resolve::HostPort,
    runner::{Deps as RunnerDeps, HostRegistry, McpRunner, RunnerConfig},
    sandbox::{Process, Sandbox, SandboxSpec},
    srt::{ServerCommand, SrtConfig, SrtSandbox, supports_state},
    sys::OsEntropy,
};
use bot_core::{
    id::ProfileId,
    port::{EffectJournal, Error, Reply},
};
use bot_gateway::active::ActiveCheck;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

/// srt 샌드박스 안 도구 프로세스의 `PATH`: srt 실행 폴더 + 운영자가 `sandbox.srt.extra_path`로 지정한 폴더 + 시스템 기본 경로(중복 제거).
fn srt_path(srt_dir: &str, extra: &[std::path::PathBuf]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |dir: &str| {
        if !dir.is_empty() && !parts.iter().any(|existing| existing == dir) {
            parts.push(dir.to_owned());
        }
    };
    push(srt_dir);
    for dir in extra {
        push(&dir.display().to_string());
    }
    for base in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        push(base);
    }
    parts.join(":")
}

/// PATH에서 실행 파일을 찾는다(내장 데모의 srt 위치).
fn on_path(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_absolute() && candidate.is_file())
}

/// 도구가 읽지 못해야 하는 곳 중 지금 있는 것: 홈·프로필 루트·호스트 설정·비밀 보관 파일·소켓·관리 비밀 파일·설치 기준.
fn denied(settings: &Settings, base: &Path) -> Vec<PathBuf> {
    let mut paths = vec![base.to_owned()];
    paths.extend(std::env::var_os("HOME").map(PathBuf::from));
    paths.extend(settings.profiles.clone());
    paths.extend(settings.host_config.clone());
    paths.extend(settings.secrets.protected_paths());
    paths.extend(settings.socket.as_deref().map(PathBuf::from));
    paths.extend(settings.admin_token_file.clone());
    paths.retain(|path| path.is_absolute() && path.exists());
    paths.sort();
    paths.dedup();
    paths
}

/// srt가 상태 요구를 지킬 수 없는(`state: required`) 서버가 있으면 Err. 판정은 srt 어댑터의 `supports_state`(호출마다 거절하는 `admit`과 같은 것)이고,
/// 호출할 때마다 거절되기 전에 도구를 실행하는 역할의 시작에서 막는다(영속 볼륨은 격리·용량·잠금을 검증한 별도 어댑터가 생긴 뒤에 쓴다).
pub(crate) fn refuse_required_state(
    servers: &BTreeMap<String, install::ServerSpec>,
) -> Result<(), String> {
    match servers.iter().find(|(_, spec)| !supports_state(spec.state)) {
        Some((alias, _)) => Err(format!(
            "servers.{alias}.state=required: srt 샌드박스는 호출 간 상태 볼륨을 지원하지 않아 시작하지 않아요 — 상태를 요구하는 서버는 영속 볼륨을 검증한 어댑터가 생긴 뒤에 쓸 수 있어요"
        )),
        None => Ok(()),
    }
}

/// `<설치 기준>/../work`: 호출별 작업 폴더를 만드는 곳의 기본값
pub(super) fn default_work(base: &Path) -> PathBuf {
    base.parent().unwrap_or(base).join("work")
}

/// 로컬 서버가 처음에 없어도 실행 중 추가되면 `SrtSandbox`를 나중에 연결할 수 있는 동적 샌드박스 래퍼.
/// 열기 전에는 아무것도 띄우지 않고(`Unavailable`), 열리면 기존 `SrtSandbox`에 그대로 위임한다.
pub(super) struct DynamicSandbox {
    inner: RwLock<Option<Arc<SrtSandbox>>>,
}

impl DynamicSandbox {
    fn empty() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }

    fn new(srt: SrtSandbox) -> Self {
        Self {
            inner: RwLock::new(Some(Arc::new(srt))),
        }
    }

    fn current(&self) -> Option<Arc<SrtSandbox>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(super) fn set(&self, srt: Arc<SrtSandbox>) {
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = Some(srt);
    }

    pub(super) fn update_servers(&self, next: BTreeMap<String, ServerCommand>) {
        if let Some(srt) = self.current() {
            srt.update_servers(next);
        }
    }
}

impl Sandbox for DynamicSandbox {
    fn spawn(&self, spec: SandboxSpec) -> Reply<'_, Process> {
        let srt = self.current();
        Box::pin(async move {
            match srt {
                Some(srt) => srt.spawn(spec).await,
                None => Err(Error::Unavailable),
            }
        })
    }

    fn effects_via_proxy(&self) -> bool {
        self.current().is_some_and(|srt| srt.effects_via_proxy())
    }

    fn admit(&self, server: &str, image: &str) -> Result<(), Error> {
        match self.current() {
            Some(srt) => srt.admit(server, image),
            None => Err(Error::Denied),
        }
    }

    fn reap(&self, owner: &str) -> Reply<'_, ()> {
        let srt = self.current();
        let owner = owner.to_owned();
        Box::pin(async move {
            match srt {
                Some(srt) => srt.reap(&owner).await,
                None => Ok(()),
            }
        })
    }
}

/// 실행 중 MCP 추가·제거 뒤 호스트 등록부·샌드박스 서버 표·설치 잠금을 함께 갱신하는 핸들.
#[derive(Clone)]
pub(super) struct LiveRunner {
    runner: Arc<McpRunner>,
    sandbox: Arc<DynamicSandbox>,
    installs: Arc<Mutex<Vec<Installs>>>,
}

impl LiveRunner {
    pub(super) fn tool_runner(&self) -> Arc<dyn ToolRunner> {
        self.runner.clone()
    }

    /// 새 호스트 설정에 맞춰 로컬 서버 설치·샌드박스 갱신까지 준비한다(실패해도 기존 실행 상태는 바뀌지 않는다).
    pub(super) async fn prepare_update(
        &self,
        host: &HostFile,
        settings: &Settings,
        uid: u32,
    ) -> Result<PreparedUpdate, String> {
        let (installs, new_sandbox, commands) = if host.servers.is_empty() {
            (Installs::default(), None, BTreeMap::new())
        } else if self.sandbox.current().is_some() {
            refuse_required_state(&host.servers)?;
            let base = install::default_base(
                settings.installs.as_deref(),
                std::env::var("HOME").ok().as_deref(),
            )?;
            let installs = install::install(&host.servers, &base, uid)?;
            let commands = server_commands(host, &installs);
            (installs, None, commands)
        } else {
            let (installs, srt) = local_sandbox(host, settings, uid).await?;
            let commands = server_commands(host, &installs);
            (installs, Some(Arc::new(srt)), commands)
        };
        let registry = host_registry(host, &installs)?;
        Ok(PreparedUpdate {
            registry,
            commands,
            new_sandbox,
            installs,
        })
    }

    /// 준비된 갱신을 즉시 적용한다(비동기 경계 없이 원자적으로 교체).
    pub(super) fn apply(&self, update: PreparedUpdate) {
        if let Some(srt) = update.new_sandbox {
            self.sandbox.set(srt);
        } else {
            self.sandbox.update_servers(update.commands);
        }
        self.runner.update_host(update.registry);
        self.installs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(update.installs);
    }
}

pub(super) struct PreparedUpdate {
    registry: HostRegistry,
    commands: BTreeMap<String, ServerCommand>,
    new_sandbox: Option<Arc<SrtSandbox>>,
    installs: Installs,
}

fn server_commands(host: &HostFile, installs: &Installs) -> BTreeMap<String, ServerCommand> {
    host.servers
        .keys()
        .filter_map(|alias| {
            installs.get(alias).map(|installed| {
                (
                    alias.clone(),
                    ServerCommand {
                        argv: installed.argv.clone(),
                        revision: installed.revision.0.clone(),
                        install_dir: installed.dir.clone(),
                        state: installed.state,
                    },
                )
            })
        })
        .collect()
}

fn host_registry(host: &HostFile, installs: &Installs) -> Result<HostRegistry, String> {
    let egress_caps: BTreeMap<ProfileId, Vec<HostPort>> = host
        .catalog
        .egress
        .iter()
        .map(|(profile, targets)| {
            let ports = targets
                .iter()
                .map(|target| {
                    HostPort::parse(target)
                        .ok_or_else(|| format!("egress.{profile}: {target}을 해석할 수 없어요"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((ProfileId(profile.clone()), ports))
        })
        .collect::<Result<_, String>>()?;
    let mut servers: BTreeMap<String, String> = installs
        .revisions()
        .into_iter()
        .map(|(alias, revision)| (alias, revision.0))
        .collect();
    servers.extend(
        host.remote_servers
            .iter()
            .map(|(alias, remote)| (alias.clone(), remote.revision())),
    );
    Ok(HostRegistry {
        servers,
        egress_caps,
        private_egress: host
            .private_egress
            .iter()
            .map(|(profile, routes)| (ProfileId(profile.clone()), routes.clone()))
            .collect(),
        remote: host.remote_servers.clone(),
    })
}

/// 호스트 설정에서 만드는 실행기 설정. 호출 시간 한도의 천장은 `limits.tool_call_max_secs`다: 활성화 때 도구 선언을 대조했고, 복원한 옛 선언은 실행기가 보내기 전에 다시 대조한다.
/// 선언하지 않은 도구의 호출 한도(60초)는 천장과 상관없다.
fn runner_config(host: &HostFile) -> RunnerConfig {
    RunnerConfig {
        max_call_timeout: Duration::from_secs(u64::from(host.limits.tool_call_max_secs)),
        ..RunnerConfig::default()
    }
}

pub(super) async fn build(
    host: &HostFile,
    settings: &Settings,
    secrets: &Arc<Watched>,
    effects: Arc<dyn EffectJournal>,
    uid: u32,
    active: Arc<dyn ActiveCheck>,
) -> Result<LiveRunner, String> {
    let (installs, sandbox) = if host.servers.is_empty() {
        (Installs::default(), Arc::new(DynamicSandbox::empty()))
    } else {
        let (installs, srt) = local_sandbox(host, settings, uid).await?;
        (installs, Arc::new(DynamicSandbox::new(srt)))
    };
    let registry = host_registry(host, &installs)?;
    let wire = super::remote::ReqwestWire::new(Arc::new(SystemResolver))?;
    let runner = McpRunner::new(
        RunnerDeps {
            sandbox: sandbox.clone(),
            secrets: secrets.clone(),
            effects,
            resolver: Arc::new(SystemResolver),
            connector: Arc::new(TcpConnector),
            entropy: Arc::new(OsEntropy),
        },
        registry,
        runner_config(host),
    )
    .with_wire(Arc::new(GuardedWire::new(Arc::new(wire), active)));
    Ok(LiveRunner {
        runner: Arc::new(runner),
        sandbox,
        installs: Arc::new(Mutex::new(vec![installs])),
    })
}

/// 로컬 서버의 읽기 전용 설치본과 srt 샌드박스.
async fn local_sandbox(
    host: &HostFile,
    settings: &Settings,
    uid: u32,
) -> Result<(Installs, SrtSandbox), String> {
    refuse_required_state(&host.servers)?;
    let base = install::default_base(
        settings.installs.as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )?;
    let installs = install::install(&host.servers, &base, uid)?;
    let srt = host.sandbox.srt.clone();
    let binary = match &srt {
            Some(srt) => srt.binary.clone(),
            // 설정이 없으면 PATH의 srt를 쓴다(내장 데모의 5분 시작)
            None => on_path("srt", std::env::var_os("PATH").as_deref()).ok_or(
                "도구를 실행하려면 srt가 필요해요 — 호스트 설정의 sandbox.srt.binary를 적거나 PATH에 srt를 두세요",
            )?,
        };
    let work_root = srt
        .as_ref()
        .map_or_else(|| default_work(&base), |srt| srt.work_root.clone());
    let srt_dir = binary
        .parent()
        .ok_or("sandbox.srt.binary에 폴더가 필요해요")?
        .display()
        .to_string();
    let servers: BTreeMap<String, ServerCommand> = host
        .servers
        .keys()
        .filter_map(|alias| {
            installs.get(alias).map(|installed| {
                (
                    alias.clone(),
                    ServerCommand {
                        argv: installed.argv.clone(),
                        revision: installed.revision.0.clone(),
                        install_dir: installed.dir.clone(),
                        state: installed.state,
                    },
                )
            })
        })
        .collect();
    let extra_path = srt
        .as_ref()
        .map_or(&[][..], |srt| srt.extra_path.as_slice());
    let sandbox = SrtSandbox::open(
            SrtConfig {
                binary,
                work_root,
                servers,
                deny_read: denied(settings, &base),
                path: srt_path(&srt_dir, extra_path),
                terminate_wait: Duration::from_secs(5),
                host_tools: None,
            },
            Arc::new(OsEntropy),
        )
        .await
        .map_err(|_| {
            format!(
                "srt를 열지 못했어요 — 고정 판본({})의 srt가 sandbox.srt.binary(또는 PATH)에 있어야 하고 작업 루트를 만들 수 있어야 해요",
                bot_broker::srt::PINNED_VERSION
            )
        })?;
    Ok((installs, sandbox))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_gateway::testing::ports::MemoryActive;

    #[test]
    fn the_work_root_defaults_next_to_the_install_base_and_denied_paths_exist_only() {
        assert_eq!(
            default_work(Path::new("/home/me/.local/state/bot/installs")),
            PathBuf::from("/home/me/.local/state/bot/work")
        );
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::read(&|name| {
            (name == "BOT_PROFILES").then(|| dir.path().join("profiles").display().to_string())
        })
        .unwrap();
        // 없는 프로필 루트는 거르고, 있는 설치 기준만 남는다(홈은 환경에 따라 있을 수 있다)
        let base = dir.path().join("installs");
        std::fs::create_dir(&base).unwrap();
        let paths = denied(&settings, &base);
        assert!(paths.contains(&base));
        assert!(!paths.contains(&dir.path().join("profiles")));
        assert!(paths.iter().all(|path| path.exists()));
    }

    /// 도구가 읽지 못하는 곳의 전체 집합을 고정한다: 설치 기준·프로필 루트·호스트 설정·소켓·관리 비밀 파일·(있으면) 홈, 그리고 파일 기반 보관소(SOPS)의 암호화 파일.
    /// 환경 변수·키체인 보관소는 도구가 읽을 파일이 따로 없다. 파일 기반 보관소를 더해도 이 집합에 자기 파일이 따라오는지 이 시험이 지킨다.
    #[test]
    fn the_denied_set_holds_the_host_files_and_the_file_of_a_file_backed_secret_store() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, "x").unwrap();
            path
        };
        let (host, socket, token, sops) = (
            file("host.json"),
            file("gateway.sock"),
            file("admin.token"),
            file("secrets.sops.json"),
        );
        let profiles = dir.path().join("profiles");
        let base = dir.path().join("installs");
        std::fs::create_dir(&profiles).unwrap();
        std::fs::create_dir(&base).unwrap();
        let settings = |secrets: &str| {
            Settings::read(&|name| match name {
                "BOT_PROFILES" => Some(profiles.display().to_string()),
                "BOT_HOST_CONFIG" => Some(host.display().to_string()),
                "BOT_SOCKET" => Some(socket.display().to_string()),
                "BOT_ADMIN_TOKEN_FILE" => Some(token.display().to_string()),
                "BOT_SECRETS" => Some(secrets.to_owned()),
                _ => None,
            })
            .unwrap()
        };
        // 홈은 시험을 돌리는 환경에 따라 있을 수 있다
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.exists());
        let expected = |secret_file: Option<&PathBuf>| {
            let mut paths = vec![
                base.clone(),
                profiles.clone(),
                host.clone(),
                socket.clone(),
                token.clone(),
            ];
            paths.extend(secret_file.cloned());
            paths.extend(home.clone());
            paths.sort();
            paths.dedup();
            paths
        };
        assert_eq!(denied(&settings("env"), &base), expected(None));
        assert_eq!(denied(&settings("keychain"), &base), expected(None));
        let sops_settings = settings(&format!("sops:{}", sops.display()));
        assert_eq!(denied(&sops_settings, &base), expected(Some(&sops)));
    }

    fn host_with(servers: &str, extra: &str) -> crate::host::HostFile {
        crate::host::parse(&format!(r#"{{"version": 1, "servers": {servers}{extra}}}"#)).unwrap()
    }

    #[test]
    fn the_runner_ceiling_follows_the_host_limits_and_nothing_else_changes() {
        let default = runner_config(&host_with("{}", ""));
        assert_eq!(default.max_call_timeout, Duration::from_secs(60));
        let raised = runner_config(&host_with(
            "{}",
            r#", "limits": {"tool_call_max_secs": 300}"#,
        ));
        assert_eq!(raised.max_call_timeout, Duration::from_secs(300));
        // 천장만 올려도 선언하지 않은 도구의 호출 한도·wall은 그대로다
        for config in [default, raised] {
            assert_eq!(config.call_timeout, Duration::from_secs(60));
            assert_eq!(config.wall, Duration::from_secs(300));
        }
    }

    #[test]
    fn a_server_that_requires_state_stops_the_start_because_srt_has_no_persistent_volume() {
        let servers = install::parse_servers(&serde_json::json!({
            "plain": {"root": "/opt/a", "command": ["bin/a"]},
            "stateful": {"root": "/opt/b", "command": ["bin/b"], "state": "required"}
        }))
        .unwrap();
        let error = refuse_required_state(&servers).unwrap_err();
        assert!(error.contains("servers.stateful.state=required"), "{error}");
        let only_plain = install::parse_servers(&serde_json::json!({
            "plain": {"root": "/opt/a", "command": ["bin/a"], "state": "ephemeral"}
        }))
        .unwrap();
        assert!(refuse_required_state(&only_plain).is_ok());
        assert!(refuse_required_state(&BTreeMap::new()).is_ok());
    }

    #[tokio::test]
    async fn building_the_runner_refuses_a_required_state_server_before_installing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("server");
        std::fs::create_dir(&root).unwrap();
        let installs = dir.path().join("installs");
        let host = crate::host::parse(&format!(
            r#"{{"version": 1, "servers": {{"stateful": {{"root": "{}", "command": ["bin/s"], "state": "required"}}}}}}"#,
            root.display()
        ))
        .unwrap();
        let settings = Settings::read(&|name| {
            (name == "BOT_INSTALLS").then(|| installs.display().to_string())
        })
        .unwrap();
        let secrets = Watched::plain(Arc::new(bot_broker::testing::FakeSecrets::new(&[])));
        let effects: Arc<dyn EffectJournal> = Arc::new(bot_broker::testing::FakeEffects::new());
        let active: Arc<dyn ActiveCheck> = MemoryActive::new(Some("set-1")).check("set-1");
        let error = build(&host, &settings, &secrets, effects, 0, active.clone())
            .await
            .err()
            .unwrap();
        assert!(error.contains("servers.stateful.state=required"), "{error}");
        assert!(!installs.exists(), "설치 폴더도 만들지 않았어요");
        // 서버가 없는 호스트는 srt도 설치도 필요 없다
        let none = crate::host::parse(r#"{"version": 1}"#).unwrap();
        let effects: Arc<dyn EffectJournal> = Arc::new(bot_broker::testing::FakeEffects::new());
        assert!(
            build(&none, &settings, &secrets, effects, 0, active)
                .await
                .is_ok()
        );
        assert!(!installs.exists());
    }

    #[test]
    fn the_srt_binary_is_found_on_the_given_path_only() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("srt");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        let joined = std::env::join_paths([Path::new("/nonexistent"), dir.path()]).unwrap();
        assert_eq!(on_path("srt", Some(&joined)), Some(tool));
        assert_eq!(on_path("absent", Some(&joined)), None);
        assert_eq!(on_path("srt", None), None);
    }

    #[test]
    fn srt_path_appends_extra_directories_and_deduplicates() {
        assert_eq!(
            srt_path("/opt/srt/bin", &[]),
            "/opt/srt/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
        assert_eq!(
            srt_path(
                "/opt/srt/bin",
                &[
                    PathBuf::from("/opt/homebrew/bin"),
                    PathBuf::from("/opt/srt/bin"),
                    PathBuf::from("/opt/tools/bin"),
                    PathBuf::from("/usr/bin"),
                ],
            ),
            "/opt/srt/bin:/opt/homebrew/bin:/opt/tools/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
    }
}
