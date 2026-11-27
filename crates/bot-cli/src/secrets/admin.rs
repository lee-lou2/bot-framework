//! SecretStore adapter for live management combining reads with SOPS/Keychain name listing, writes, and cache invalidation.
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

/// Shares one backend instance across reads, live writes, and cache invalidation; never leaks secret values.
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

    pub fn invalidate(&self, profile: &ProfileId) {
        if let Backend::Sops(sops) = &self.backend {
            sops.invalidate(profile);
        }
    }

    /// Lists secret names present in the store (values are never returned).
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

    pub async fn set_secret(
        &self,
        profile: &ProfileId,
        name: &SecretName,
        value: &str,
    ) -> Result<(), String> {
        match &self.backend {
            Backend::Sops(sops) => sops.set_secret(profile, name, value).await.map_err(|e| match e {
                Error::Missing => format!(
                    "SOPS file ({}) does not exist — create an initial age-encrypted SOPS JSON file first",
                    sops.file().display()
                ),
                Error::Invalid => {
                    "secret value cannot be empty or contain newlines or NUL bytes".into()
                }
                _ => "failed to store secret in SOPS file (check the sops binary and age key)"
                    .into(),
            }),
            Backend::Keychain(keychain) => keychain
                .set_secret(profile, name, value)
                .await
                .map_err(|_| "failed to store secret in macOS Keychain".into()),
            Backend::Env(_) => Err(format!(
                "environment store (BOT_SECRETS=env) does not support runtime secret writes — set env var {} or use SOPS (BOT_SECRETS=sops:<file>)",
                super::env::variable_name(&profile.0, name.as_str())
            )),
            Backend::Custom(_) => Err("this secret store does not support runtime writes".into()),
        }
    }

    pub async fn unset_secret(&self, profile: &ProfileId, name: &SecretName) -> Result<(), String> {
        match &self.backend {
            Backend::Sops(sops) => sops
                .unset_secret(profile, name)
                .await
                .map_err(|_| "failed to remove secret from SOPS file".into()),
            Backend::Keychain(keychain) => keychain
                .unset_secret(profile, name)
                .await
                .map_err(|_| "failed to remove secret from macOS Keychain".into()),
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
