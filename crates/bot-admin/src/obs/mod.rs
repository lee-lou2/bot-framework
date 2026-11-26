//! 관측 내보내기 공통 기반: HTTP/HTTPS 전송(`transport`), OTLP protobuf 선 형식(`proto`),
//! 큐·차단기·활성화 문·비밀 참조(`runtime`). `trace`·`logs`·`issues`가 같은 계약을 공유한다.

pub(crate) mod proto;
pub(crate) mod runtime;
pub(crate) mod transport;

use bot_core::id::{ProfileId, SecretName};

/// 호스트 단위 관측 설정(`admin.logs`·`admin.issues`)에서 `profile`을 생략할 때 쓰는 기본 SecretStore 이름 공간.
pub const DEFAULT_SECRET_PROFILE: &str = "host";

/// 호스트 단위 관측 설정이 SecretStore에서 읽을 자격 위치(`bot/<profile>/<name>`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretRef {
    pub profile: ProfileId,
    pub name: SecretName,
}

impl SecretRef {
    pub fn host(name: SecretName) -> Self {
        Self {
            profile: ProfileId(DEFAULT_SECRET_PROFILE.to_owned()),
            name,
        }
    }

    pub fn in_profile(profile: ProfileId, name: SecretName) -> Self {
        Self { profile, name }
    }
}

/// 호스트 단위 내보내기 헤더 값: 평문(비밀이 아닌 것) 또는 SecretStore 참조.
#[derive(Clone, PartialEq)]
pub enum HostHeaderValue {
    Plain(String),
    Secret(SecretRef),
}

/// 진단 출력에 평문 헤더 값을 싣지 않는다(실수로 넣은 토큰이 로그에 남지 않게).
impl std::fmt::Debug for HostHeaderValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain(_) => f.write_str("Plain(..)"),
            Self::Secret(secret) => {
                write!(f, "Secret({}/{})", secret.profile.0, secret.name.as_str())
            }
        }
    }
}

pub use runtime::{Options, Stats};
