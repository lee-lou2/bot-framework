//! 도구 실행기(srt 샌드박스 안의 MCP 서버)와 읽기 전용 설치본. worker의 브로커와 민감 DM 접수(`PrivateGate`)가 같은 방식으로 만든다.
//!
//! 호스트에서 계산하는 값: srt 실행 파일(`sandbox.srt.binary`, 내장 데모는 PATH의 `srt`)·작업 루트(`sandbox.srt.work_root`, 기본은
//! 설치 폴더 옆 `work`)·srt가 쓰는 PATH(srt 폴더 + 시스템 기본)·도구가 읽지 못하는 곳(홈·프로필 루트·호스트 설정·비밀 파일·소켓·설치 기준).
use super::{decide::InstalledOnly, watched::Watched};
use crate::{
    host::HostFile,
    install::{self, Installs},
    settings::{SecretsKind, Settings},
};
use bot_broker::{
    ports::ToolRunner,
    proxy::{SystemResolver, TcpConnector},
    resolve::HostPort,
    runner::{Deps as RunnerDeps, HostRegistry, McpRunner, RunnerConfig},
    srt::{ServerCommand, SrtConfig, SrtSandbox},
    sys::OsEntropy,
};
use bot_core::{id::ProfileId, install::ServerState, port::EffectJournal};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

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
    if let SecretsKind::Sops(file) = &settings.secrets {
        paths.push(file.clone());
    }
    paths.extend(settings.socket.as_deref().map(PathBuf::from));
    paths.extend(settings.admin_token_file.clone());
    paths.retain(|path| path.is_absolute() && path.exists());
    paths.sort();
    paths.dedup();
    paths
}

/// 호출 간 상태 볼륨을 요구하는(`state: required`) 서버가 있으면 Err. srt 샌드박스는 영속 볼륨을 지원하지 않으므로, 호출할 때마다 거절되기
/// 전에 도구를 실행하는 역할의 시작에서 막는다(영속 볼륨은 격리·용량·잠금을 검증한 별도 어댑터가 생긴 뒤에 쓴다).
fn refuse_required_state(servers: &BTreeMap<String, install::ServerSpec>) -> Result<(), String> {
    match servers
        .iter()
        .find(|(_, spec)| spec.state == ServerState::Required)
    {
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

/// 도구 실행기와 그 설치본(쓰는 동안 설치본 사용 잠금을 쥔다). 서버가 없는 호스트는 실행기 없이(설치 판본 조회만) 만든다.
pub(super) async fn build(
    host: &HostFile,
    settings: &Settings,
    secrets: &Arc<Watched>,
    effects: Arc<dyn EffectJournal>,
    uid: u32,
) -> Result<(Arc<dyn ToolRunner>, Installs), String> {
    if host.servers.is_empty() {
        return Ok((
            Arc::new(InstalledOnly(BTreeMap::new())),
            Installs::default(),
        ));
    }
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
    let sandbox = SrtSandbox::open(
            SrtConfig {
                binary,
                work_root,
                servers,
                deny_read: denied(settings, &base),
                path: format!("{srt_dir}:/usr/bin:/bin:/usr/sbin:/sbin"),
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
    let runner = McpRunner::new(
        RunnerDeps {
            sandbox: Arc::new(sandbox),
            secrets: secrets.clone(),
            effects,
            resolver: Arc::new(SystemResolver),
            connector: Arc::new(TcpConnector),
            entropy: Arc::new(OsEntropy),
        },
        HostRegistry {
            servers: installs
                .revisions()
                .into_iter()
                .map(|(alias, revision)| (alias, revision.0))
                .collect(),
            egress_caps,
        },
        RunnerConfig::default(),
    );
    Ok((Arc::new(runner), installs))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let error = build(&host, &settings, &secrets, effects, 0)
            .await
            .err()
            .unwrap();
        assert!(error.contains("servers.stateful.state=required"), "{error}");
        assert!(!installs.exists(), "설치 폴더도 만들지 않았어요");
        // 서버가 없는 호스트는 srt도 설치도 필요 없다
        let none = crate::host::parse(r#"{"version": 1}"#).unwrap();
        let effects: Arc<dyn EffectJournal> = Arc::new(bot_broker::testing::FakeEffects::new());
        assert!(build(&none, &settings, &secrets, effects, 0).await.is_ok());
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
}
