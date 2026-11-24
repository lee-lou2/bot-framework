//! SecretStore 구현. 논리 이름을 값으로 풀 뿐이고, 호출자가 프로필이나 전체 키를 넘길 수 없다.
//! 저장 위치는 `bot/<profile>/<name>` 한 가지 이름 공간이며 전역 이름 폴백은 없다.
//!
//! - [`env::EnvStore`] — 개발용. 신뢰 호스트의 환경에서만 읽는다.
//! - [`keychain::KeychainStore`] — macOS 키체인(`security` 명령).
//! - [`sops::SopsStore`] — SOPS/age 암호화 JSON 파일(`sops --decrypt`).
//!
//! 값은 Debug·Display를 구현하지 않는 `SecretValue`로만 오간다. 오류에는 이름·분류만 싣고 값은 싣지 않는다.
pub mod env;
pub mod keychain;
pub mod sops;

use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretValue},
};
use std::{collections::BTreeMap, sync::Arc};

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
