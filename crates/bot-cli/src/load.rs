//! 명령이 함께 쓰는 로딩: 호스트 등록부 → 프로필 집합 검증 → 입구 표 검사. 어느 단계든 실패하면 아무것도 시작하지 않는다.
use crate::{
    host::{self, HostFile},
    secrets::{env::EnvStore, keychain::KeychainStore, sops::SopsStore},
    settings::{SecretsKind, Settings},
};
use bot_core::{port::SecretStore, profile::ProfileCatalog};
use bot_gateway::{
    cli::socket::current_uid,
    host::resolve_uid,
    routes::{Active, Routes},
};
use bot_profile::{Limits, ProfileSet, Registry};
use std::sync::Arc;

pub struct Loaded {
    pub host: Arc<HostFile>,
    pub registry: Arc<Registry>,
    pub set: Arc<ProfileSet>,
    /// 이 프로세스를 실행한 OS 사용자
    pub uid: u32,
}

/// 호스트 등록부를 읽는다. `BOT_HOST_CONFIG`가 없으면 내장 데모 호스트다.
pub fn load_host(settings: &Settings, uid: u32) -> Result<HostFile, String> {
    let mut host = match &settings.host_config {
        Some(path) => host::load(path).map_err(|e| e.to_string())?,
        None => {
            let mut demo = host::demo(uid);
            demo.attach_demo_server(settings.profiles.as_deref());
            demo
        }
    };
    // OS 사용자 이름은 여기서 한 번만 풀어 고정한다(지문·입구 표·관리 신원이 같은 표를 쓴다)
    host.pin_uids()?;
    Ok(host)
}

pub fn load(settings: &Settings) -> Result<Loaded, String> {
    let root = settings
        .profiles
        .as_ref()
        .ok_or("BOT_PROFILES가 필요해요(프로필 폴더 경로)")?;
    let uid = current_uid().map_err(|e| e.to_string())?;
    let host = load_host(settings, uid)?;
    let registry = Arc::new(Registry::new());
    let set = registry
        .reload(root, &host.catalog, &Limits::default())
        .map_err(|report| report.to_string())?;
    Ok(Loaded {
        host: Arc::new(host),
        registry,
        set,
        uid,
    })
}

/// 활성 프로필 집합 + 호스트 gateway 절로 입구 표를 만들어 본다(별칭 해석·입구 유일성·트리거 검사).
pub fn check_entrances(active: Active, host: &HostFile) -> Result<Routes, String> {
    Routes::build(active, &host.gateway, &resolve_uid).map_err(|e| e.to_string())
}

/// 활성 집합의 스냅샷 맵
pub fn active_of(registry: &Registry) -> Active {
    registry.active()
}

/// 설정한 보관소로 SecretStore를 만든다.
pub fn secret_store(settings: &Settings) -> Result<Arc<dyn SecretStore>, String> {
    Ok(match &settings.secrets {
        SecretsKind::Env => Arc::new(EnvStore::from_process()),
        SecretsKind::Keychain => Arc::new(KeychainStore::new()?),
        SecretsKind::Sops(file) => Arc::new(SopsStore::new(file.clone())),
    })
}
