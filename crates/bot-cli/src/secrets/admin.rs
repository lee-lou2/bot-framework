//! 런타임 관리기가 쓰는 비밀 보관소 어댑터: `SecretStore` 읽기와 함께 SOPS/키체인의 이름 조회·암호화 저장·제거·캐시 무효화를 같은 인스턴스로 처리한다.
//! 비밀 값은 어떤 반환값·오류에도 싣지 않는다.
use super::{env::EnvStore, keychain::KeychainStore, sops::SopsStore};
use crate::settings::{SecretsKind, Settings};
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretStore},
};
use std::{collections::BTreeSet, sync::Arc};

enum Backend {
    Env(Arc<EnvStore>),
    Keychain(Arc<KeychainStore>),
    Sops(Arc<SopsStore>),
    Custom(Arc<dyn SecretStore>),
}

/// 읽기와 관리 쓰기·캐시 무효화가 같은 보관소 인스턴스를 공유한다.
pub struct SecretAdmin {
    backend: Backend,
}

impl SecretAdmin {
    pub fn from_settings(settings: &Settings) -> Result<Self, String> {
        let backend = match &settings.secrets {
            SecretsKind::Env => Backend::Env(Arc::new(EnvStore::from_process())),
            SecretsKind::Keychain => Backend::Keychain(Arc::new(KeychainStore::new()?)),
            SecretsKind::Sops(file) => Backend::Sops(Arc::new(SopsStore::new(file.clone()))),
        };
        Ok(Self { backend })
    }

    pub fn from_sops(store: Arc<SopsStore>) -> Self {
        Self {
            backend: Backend::Sops(store),
        }
    }

    pub fn custom(store: Arc<dyn SecretStore>) -> Self {
        Self {
            backend: Backend::Custom(store),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match &self.backend {
            Backend::Env(_) => "env",
            Backend::Keychain(_) => "keychain",
            Backend::Sops(_) => "sops",
            Backend::Custom(_) => "custom",
        }
    }

    /// SOPS 캐시를 즉시 비워 외부에서 바뀐 암호화 파일을 바로 읽게 한다.
    pub fn invalidate(&self, profile: &ProfileId) {
        if let Backend::Sops(sops) = &self.backend {
            sops.invalidate(profile);
        }
    }

    /// 보관소에 값이 들어 있는 비밀 **이름** 목록(값은 절대 반환하지 않는다).
    pub async fn list_names(
        &self,
        profile: &ProfileId,
        candidates: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>, Error> {
        match &self.backend {
            Backend::Sops(sops) => sops.list_names(profile).await,
            Backend::Env(_) => {
                let prefix = format!("BOT_SECRET__{}__", profile.0.replace('-', "_"));
                let mut found = BTreeSet::new();
                for (key, value) in std::env::vars() {
                    if value.is_empty() {
                        continue;
                    }
                    if let Some(rest) = key.strip_prefix(&prefix) {
                        let candidate = rest.to_ascii_lowercase().replace('_', "-");
                        if SecretName::parse(&candidate).is_some() {
                            found.insert(candidate);
                        }
                    }
                }
                Ok(found)
            }
            Backend::Keychain(_) | Backend::Custom(_) => {
                let mut found = BTreeSet::new();
                let Ok(handle) = self.for_profile(profile.clone()).await else {
                    return Ok(found);
                };
                for name in candidates {
                    if let Some(parsed) = SecretName::parse(name)
                        && handle.resolve(parsed).await.is_ok()
                    {
                        found.insert(name.clone());
                    }
                }
                Ok(found)
            }
        }
    }

    /// 비밀 값을 현재 보관소(SOPS/키체인)에 저장한다. `env` 보관소는 런타임 프로세스 환경을 고치지 않고 안내 오류를 돌려준다.
    pub async fn set_secret(
        &self,
        profile: &ProfileId,
        name: &SecretName,
        value: &str,
    ) -> Result<(), String> {
        match &self.backend {
            Backend::Sops(sops) => sops.set_secret(profile, name, value).await.map_err(|e| match e {
                Error::Missing => format!(
                    "SOPS 암호화 파일({})이 없어요 — 먼저 age 수신자로 초기 SOPS JSON 파일을 만든 뒤 다시 시도하세요",
                    sops.file().display()
                ),
                Error::Invalid => "비밀 값이 비어 있거나 줄바꿈·NUL이 들어 있어 저장할 수 없어요".into(),
                _ => "SOPS 암호화 파일에 비밀을 저장하지 못했어요(sops 명령·age 키를 확인하세요)".into(),
            }),
            Backend::Keychain(keychain) => keychain
                .set_secret(profile, name, value)
                .await
                .map_err(|_| "macOS 키체인에 비밀을 저장하지 못했어요".into()),
            Backend::Env(_) => Err(format!(
                "환경 변수 보관소(BOT_SECRETS=env)는 런타임에 비밀을 저장할 수 없어요 — 환경 변수 {}를 설정하거나 SOPS(BOT_SECRETS=sops:<파일>)를 사용하세요",
                super::env::variable_name(&profile.0, name.as_str())
            )),
            Backend::Custom(_) => Err("이 보관소는 런타임 비밀 저장을 지원하지 않아요".into()),
        }
    }

    /// 비밀 항목을 현재 보관소(SOPS/키체인)에서 제거한다.
    pub async fn unset_secret(&self, profile: &ProfileId, name: &SecretName) -> Result<(), String> {
        match &self.backend {
            Backend::Sops(sops) => sops
                .unset_secret(profile, name)
                .await
                .map_err(|_| "SOPS 암호화 파일에서 비밀을 제거하지 못했어요".into()),
            Backend::Keychain(keychain) => keychain
                .unset_secret(profile, name)
                .await
                .map_err(|_| "macOS 키체인에서 비밀을 제거하지 못했어요".into()),
            Backend::Env(_) | Backend::Custom(_) => Ok(()),
        }
    }
}

impl SecretStore for SecretAdmin {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        match &self.backend {
            Backend::Env(store) => store.for_profile(profile),
            Backend::Keychain(store) => store.for_profile(profile),
            Backend::Sops(store) => store.for_profile(profile),
            Backend::Custom(store) => store.for_profile(profile),
        }
    }
}
