//! API 키 인증. 키 값 하나가 입구 별칭과 주체를 정한다(URL에는 별칭이 없다).
//!
//! - 헤더에는 표준 `Authorization: Bearer <키>` 한 개만 허용한다(헤더가 여럿이면 거절). 키는 ASCII 32~256바이트다.
//! - 호스트가 연 모든 OpenAI 입구의 모든 키 슬롯 값을 **지금** 비밀 보관소(Watched 래퍼)에서 읽어, 값의 SHA-256을 고정 길이로 상수 시간 대조한다.
//!   값이 맞아도 멈추지 않고 모든 슬롯을 끝까지 본다. 한 키 값이 슬롯 둘 이상에 맞으면 실패다(회전용 슬롯은 서로 다른 값을 가진다).
//! - 값은 이 요청 안에서만 쓰고 기억하지 않는다: 회전·삭제가 다음 요청부터 바로 반영되고, 기억이 보관소 갱신을 넘어 수명을 늘리지 않는다
//!   (보관소 자체의 짧은 기억은 그대로 따른다). 가림 목록의 옛 값은 인증에 쓰지 않는다.
//! - 어느 슬롯이든 **읽지 못해**(보관소 장애·시간 초과) 유일성을 확인할 수 없으면 닫는다(503). 키가 맞는지와 무관한 응답이라 키의 유효 여부를 알려 주지 않는다.
//!   보관소에 값이 **없는** 슬롯은 누구와도 겹칠 수 없으니 그 슬롯만 쓰지 않는다(다른 키의 인증은 계속된다).
use super::super::HttpGateway;
use crate::{host::openai_key_shaped, identity, routes::Routes};
use axum::http::{HeaderMap, header};
use bot_core::{
    id::{PrincipalId, SecretName},
    port::Error,
};
use futures_util::future::join_all;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::time::timeout;

/// 슬롯 값 하나(또는 프로필 핸들)를 읽는 데 주는 시간. 잠긴 키체인·응답 없는 보관소가 요청을 붙들지 못하게 한다.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

type Hash = [u8; 32];

/// 키 슬롯 하나를 읽은 결과
enum Slot {
    Value(Vec<u8>),
    /// 보관소에 값이 없다(또는 자격 이름이 올바르지 않다)
    Absent,
    /// 보관소 장애·시간 초과
    Unreadable,
}

/// 인증된 호출자. 키 값은 들고 있지 않다(해시만 — 전달 직전 재확인용).
pub(super) struct Authed {
    pub endpoint: String,
    pub subject: String,
    pub principal: PrincipalId,
    hash: Hash,
}

impl Authed {
    /// 시험용: 키 값 없이 입구·주체만 가진 인증된 호출자
    #[cfg(test)]
    pub(super) fn for_test(endpoint: &str, subject: &str) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            subject: subject.to_owned(),
            principal: identity::http(endpoint, subject).expect("올바른 주체"),
            hash: [0; 32],
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum AuthError {
    /// 키가 없거나 모양이 틀렸거나 어느 슬롯에도(또는 둘 이상에) 맞지 않는다.
    Invalid,
    /// 유일성을 확인하지 못했다(보관소 장애·슬롯 이름 오류).
    Unavailable,
}

/// 표준 Bearer 키 한 개. `Authorization` 헤더가 둘 이상이면 어느 것도 믿지 않는다.
fn presented(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.to_str().ok()?.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && openai_key_shaped(token.as_bytes())).then_some(token)
}

/// 고정 길이 해시의 상수 시간 비교(첫 차이에서 멈추지 않는다)
fn same(a: &Hash, b: &Hash) -> bool {
    std::hint::black_box(a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y))) == 0
}

impl HttpGateway {
    pub(super) async fn openai_authenticate(
        &self,
        routes: &Routes,
        headers: &HeaderMap,
    ) -> Result<Authed, AuthError> {
        let token = presented(headers).ok_or(AuthError::Invalid)?;
        let hash: Hash = Sha256::digest(token.as_bytes()).into();
        let (endpoint, subject) = self.match_key(routes, hash).await?;
        // 호스트 설정 검증이 보장한 모양이다. 어긋나면 설정 오류이므로 열지 않는다.
        let principal = identity::http(&endpoint, &subject).ok_or(AuthError::Unavailable)?;
        Ok(Authed {
            endpoint,
            subject,
            principal,
            hash,
        })
    }

    /// 결과·안내를 내보내기 직전의 재확인: 같은 키 값이 지금도 같은 입구·주체를 정확히 하나로 가리키는가.
    /// 키가 회전돼 같은 주체의 다른 슬롯으로 옮겨졌으면 통과한다(주체의 작업은 키가 아니라 주체에 묶인다).
    pub(super) async fn openai_reauthenticate(&self, authed: &Authed) -> Result<(), AuthError> {
        let routes = self.ingress.routes().map_err(|_| AuthError::Unavailable)?;
        let (endpoint, subject) = self.match_key(&routes, authed.hash).await?;
        if endpoint == authed.endpoint && subject == authed.subject {
            Ok(())
        } else {
            Err(AuthError::Invalid)
        }
    }

    async fn match_key(
        &self,
        routes: &Routes,
        presented: Hash,
    ) -> Result<(String, String), AuthError> {
        let host = self.ingress.host().clone();
        let mut matched = 0usize;
        let mut found = None;
        let mut unverifiable = false;
        for (alias, endpoint) in &host.http.openai {
            // 쓰는 프로필이 없는 입구는 자격 이름 공간이 없어 누구도 인증하지 못한다
            let Some(profile) = routes.openai_profile(alias) else {
                continue;
            };
            let handle = match timeout(READ_TIMEOUT, self.secrets.for_profile(profile.clone()))
                .await
            {
                Ok(Ok(handle)) => handle,
                _ => {
                    tracing::error!(endpoint = %alias, "OpenAI 입구의 비밀 보관소 핸들을 얻지 못했어요");
                    unverifiable = true;
                    continue;
                }
            };
            let reads = endpoint.keys.iter().map(|(slot, key)| {
                let handle = &handle;
                async move {
                    let Some(name) = SecretName::parse(&key.secret) else {
                        return (slot, key, Slot::Absent);
                    };
                    match timeout(READ_TIMEOUT, handle.resolve(name)).await {
                        Ok(Ok(value)) => (slot, key, Slot::Value(value.0)),
                        // 값이 없는 슬롯은 누구와도 겹칠 수 없다(그 슬롯으로는 아무도 인증하지 못할 뿐이다)
                        Ok(Err(Error::Missing)) => (slot, key, Slot::Absent),
                        // 읽지 못한 슬롯은 다른 슬롯과 겹치는지 알 수 없다
                        _ => (slot, key, Slot::Unreadable),
                    }
                }
            });
            for (slot, key, read) in join_all(reads).await {
                let value = match read {
                    Slot::Value(value) => value,
                    Slot::Absent => {
                        tracing::error!(endpoint = %alias, slot = %slot, "OpenAI 입구의 키 슬롯 값이 보관소에 없어 그 슬롯으로는 인증하지 않아요");
                        continue;
                    }
                    Slot::Unreadable => {
                        tracing::error!(endpoint = %alias, slot = %slot, "OpenAI 입구의 키 슬롯 값을 읽지 못해 인증을 닫아요");
                        unverifiable = true;
                        continue;
                    }
                };
                // 보관소 파일 끝의 줄바꿈 등은 값이 아니다(가림 장치도 앞뒤 공백을 뗀 모양을 함께 가린다)
                let secret = value.trim_ascii();
                if !openai_key_shaped(secret) {
                    tracing::error!(endpoint = %alias, slot = %slot, "OpenAI 입구의 키 슬롯 값이 키 모양(ASCII 32~256바이트)이 아니라 쓰지 않아요");
                    continue;
                }
                let hash: Hash = Sha256::digest(secret).into();
                if same(&hash, &presented) {
                    matched += 1;
                    found = Some((alias.clone(), key.subject.clone()));
                }
            }
        }
        if unverifiable {
            return Err(AuthError::Unavailable);
        }
        match (matched, found) {
            (1, Some(found)) => Ok(found),
            _ => Err(AuthError::Invalid),
        }
    }
}

#[cfg(test)]
mod tests;
