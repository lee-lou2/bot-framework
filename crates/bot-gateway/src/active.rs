//! 이 프로세스가 쓰는 프로필 집합이 저장소가 게시한 활성 집합과 같은지 확인한다.
//!
//! 접수·결과 반환의 권한 검사는 이 프로세스의 로컬 표로 한다. 다른 프로세스가 새 판본을 활성화했다면 이 프로세스는
//! 구판이다. 구판은 새 접수도 결과 반환도 하지 않고 일시 실패로 닫는다(HTTP 503·CLI `unavailable`·Slack ack 보류) —
//! 상대가 다시 시도하고, 운영자는 이 프로세스를 새 판본으로 다시 시작한다. 새 접수의 최종 대조(활성 판본 교체와
//! 같은 잠금 아래)는 저장소가 한다. 여기는 그 앞에서 구판 프로세스를 일찍 닫고, 저장소가 대조하지 않는 결과 반환을 지킨다.
use bot_core::{
    id::Revision,
    port::{Error, Reply},
    profile::ActiveRevisions,
};
use std::sync::Arc;

pub trait ActiveCheck: Send + Sync {
    /// 이 프로세스의 집합이 저장소의 활성 집합과 같으면 Ok, 다르거나 아직 게시되지 않았으면 `Stale`,
    /// 저장소를 읽지 못하면 `Unavailable`.
    fn verify(&self) -> Reply<'_, ()>;
}

/// 시작할 때 검증한 프로필 집합의 판본을 저장소의 활성 판본과 견준다.
pub struct ActiveSet {
    store: Arc<dyn ActiveRevisions>,
    expected: Revision,
}

impl ActiveSet {
    pub fn new(store: Arc<dyn ActiveRevisions>, expected: Revision) -> Self {
        Self { store, expected }
    }
}

impl ActiveCheck for ActiveSet {
    fn verify(&self) -> Reply<'_, ()> {
        Box::pin(async move {
            match self.store.current().await? {
                Some(current) if current == self.expected => Ok(()),
                _ => Err(Error::Stale),
            }
        })
    }
}

/// 응답(결과·상태) 직전 확인의 결과
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Release {
    /// 지금도 멤버이고 이 프로세스의 판본이 활성 판본이다.
    Allowed,
    /// 접수 뒤 멤버가 철회됐거나 입구 소유자가 바뀌었다. 요청이 있는지도 알리지 않는다.
    Forbidden,
    /// 이 프로세스의 판본을 확인하지 못했거나 구판이다. 일시 실패로 닫는다.
    Unavailable,
}
