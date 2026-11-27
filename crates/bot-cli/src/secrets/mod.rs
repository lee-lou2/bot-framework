//! SecretStore 구현. 논리 이름을 값으로 풀 뿐이고, 호출자가 프로필이나 전체 키를 넘길 수 없다.
//! 저장 위치는 `bot/<profile>/<name>` 한 가지 이름 공간이며 전역 이름 폴백은 없다.
//!
//! - [`env::EnvStore`] — 개발용. 신뢰 호스트의 환경에서만 읽는다.
//! - [`keychain::KeychainStore`] — macOS 키체인(`security` 명령).
//! - [`sops::SopsStore`] — SOPS/age 암호화 JSON 파일(`sops --decrypt`).
//!
//! 값은 Debug·Display를 구현하지 않는 `SecretValue`로만 오간다. 오류에는 이름·분류만 싣고 값은 싣지 않는다.
//!
//! 백엔드 종류(`SecretsKind`)마다 달라지는 **사실**은 아래 `impl SecretsKind`에 모았다: 도구가 읽지 못해야 하는 파일·자격이 없을 때의 힌트·복호화 필요 여부.
//! 조립 코드(`serve::runner`·`validate`·`doctor`)는 종류를 직접 가르지 않고 이 메서드를 부른다. 새 백엔드를 더하면 컴파일러가 이 블록의 `match`를 모두 가리킨다.
pub mod admin;
pub mod env;
pub mod keychain;
pub mod sops;

use crate::settings::SecretsKind;
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretValue},
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

impl SecretsKind {
    /// 도구가 읽지 못해야 하는 이 보관소의 파일. 파일 기반 보관소는 자기 파일을 여기서 알린다 — 조립 코드는 종류를 가르지 않고 이 목록을 읽기 금지 목록에 그대로 더한다.
    /// 환경 변수 보관소는 도구가 환경을 물려받지 않고, 키체인은 홈 폴더 밑이라 따로 없다.
    pub fn protected_paths(&self) -> Vec<PathBuf> {
        match self {
            SecretsKind::Env | SecretsKind::Keychain => Vec::new(),
            SecretsKind::Sops(file) => vec![file.clone()],
        }
    }

    /// 자격이 없다고 알릴 때 붙이는 힌트(환경 변수 보관소는 읽는 변수 이름). 값은 싣지 않는다.
    pub fn missing_hint(&self, profile: &str, name: &str) -> String {
        match self {
            SecretsKind::Env => format!(" (환경 변수 {})", env::variable_name(profile, name)),
            SecretsKind::Keychain | SecretsKind::Sops(_) => String::new(),
        }
    }

    /// 값을 읽으려면 복호화(외부 프로그램·키 서비스 접속)가 필요한 보관소의 프로그램 이름. `bot doctor`는 그런 보관소를 열지 않고 «미검사»로 둔다.
    pub fn decryptor(&self) -> Option<&'static str> {
        match self {
            SecretsKind::Env | SecretsKind::Keychain => None,
            SecretsKind::Sops(_) => Some("sops"),
        }
    }
}

/// 프로필 하나의 이름 공간을 담은 핸들. 만들어질 때 프로필이 고정되어 다른 프로필을 읽을 방법이 없다.
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

/// 호스트가 확인하지 않은 프로필 문자열을 거른다.
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
        Poll::Pending => panic!("이 시험의 future는 즉시 끝나야 해요"),
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
            format!(" (환경 변수 {})", env::variable_name("personal", "api-key"))
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
