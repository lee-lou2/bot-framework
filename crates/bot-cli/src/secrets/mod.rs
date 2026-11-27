//! `SecretStore` backends (env, macOS Keychain, SOPS) scoped strictly to `bot/<profile>/<name>`.
// Values only travel inside opaque `SecretValue`; errors carry name/class only, never values.
pub mod admin;
pub(crate) mod env;
mod keychain;
pub mod sops;

use crate::settings::SecretsKind;
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretValue},
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

impl SecretsKind {
    /// Files owned by this backend that sandboxed tools must be denied read access to.
    pub fn protected_paths(&self) -> Vec<PathBuf> {
        match self {
            SecretsKind::Env | SecretsKind::Keychain => Vec::new(),
            SecretsKind::Sops(file) => vec![file.clone()],
        }
    }

    pub fn missing_hint(&self, profile: &str, name: &str) -> String {
        match self {
            SecretsKind::Env => format!(" (env {})", env::variable_name(profile, name)),
            SecretsKind::Keychain | SecretsKind::Sops(_) => String::new(),
        }
    }

    /// External decryptor program if reading requires decryption (`bot doctor` skips those checks).
    pub fn decryptor(&self) -> Option<&'static str> {
        match self {
            SecretsKind::Env | SecretsKind::Keychain => None,
            SecretsKind::Sops(_) => Some("sops"),
        }
    }
}

/// Profile-scoped secret handle; cannot query outside the bound profile namespace.
pub(crate) struct Namespace {
    values: Arc<BTreeMap<String, Vec<u8>>>,
}

impl Namespace {
    pub(crate) fn new(values: BTreeMap<String, Vec<u8>>) -> Self {
        Self {
            values: Arc::new(values),
        }
    }
}

impl ProfileSecrets for Namespace {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        let found = self
            .values
            .get(name.as_str())
            .filter(|value| !value.is_empty())
            .map(|value| SecretValue(value.clone()));
        Box::pin(std::future::ready(found.ok_or(Error::Missing)))
    }
}

pub(crate) fn checked_profile(profile: &ProfileId) -> Result<&str, Error> {
    if bot_core::id::is_name(&profile.0) {
        Ok(&profile.0)
    } else {
        Err(Error::Invalid)
    }
}

#[cfg(test)]
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test future must resolve immediately"),
    }
}

#[cfg(test)]
mod kind_tests {
    use super::*;

    const KINDS: fn() -> [SecretsKind; 3] = || {
        [
            SecretsKind::Env,
            SecretsKind::Keychain,
            SecretsKind::Sops("/srv/secrets.sops.json".into()),
        ]
    };

    #[test]
    fn only_a_file_backed_store_names_a_file_the_tools_must_not_read() {
        let [env, keychain, sops] = KINDS();
        assert!(env.protected_paths().is_empty() && keychain.protected_paths().is_empty());
        assert_eq!(
            sops.protected_paths(),
            vec![PathBuf::from("/srv/secrets.sops.json")]
        );
    }

    #[test]
    fn only_the_environment_store_hints_at_a_variable_name_and_never_at_a_value() {
        let [env, keychain, sops] = KINDS();
        assert_eq!(
            env.missing_hint("personal", "api-key"),
            format!(" (env {})", env::variable_name("personal", "api-key"))
        );
        assert!(env.missing_hint("personal", "api-key").contains("api_key"));
        assert_eq!(keychain.missing_hint("personal", "api-key"), "");
        assert_eq!(sops.missing_hint("personal", "api-key"), "");
    }

    #[test]
    fn only_the_sops_store_needs_a_decryptor() {
        let [env, keychain, sops] = KINDS();
        assert_eq!(
            (env.decryptor(), keychain.decryptor(), sops.decryptor()),
            (None, None, Some("sops"))
        );
    }
}
