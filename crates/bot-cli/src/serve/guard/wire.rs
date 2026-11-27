//! 원격 MCP 서버로 나가는 요청 직전의 판본 확인(`GuardedWire`). 모델 상류의 `GuardedUpstream`과 같은 보장이다(`guard.rs`의 실행 저장소·브로커 래퍼와 한 짝).
//!
//! 브로커는 호출을 시작하기 전에 활성 판본을 확인하지만, 원격 서버와는 요청이 여럿 오간다(판본 확인·도구 목록·구판의 초기화·`tools/call`). 승인 뒤 서버가 느려
//! 목록을 기다리는 사이 다른 프로세스가 목적지를 철회한 새 집합을 게시하면, 구판 워커가 옛 주소로 bearer와 쓰기 호출을 보낼 수 있다. 그래서 **요청마다**(자격을 읽은 뒤,
//! 연결을 맺기 전에) 이 프로세스의 판본이 아직 활성인지 다시 확인하고, 아니면(또는 확인하지 못하면) 아무것도 보내지 않는다(`WireFail::Stale`).
use bot_broker::mcp::wire::{RemoteWire, WireFail, WireReply, WireRequest};
use bot_core::active::ActiveCheck;
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, timeout_at};

/// 확인이 끝나지 않을 때 요청을 붙잡아 두는 한도. 넘기면 보내지 않는다(확인하지 못한 판본으로 나가지 않는다).
const GRACE: Duration = Duration::from_secs(10);

pub struct GuardedWire {
    inner: Arc<dyn RemoteWire>,
    active: Arc<dyn ActiveCheck>,
    grace: Duration,
}

impl GuardedWire {
    pub fn new(inner: Arc<dyn RemoteWire>, active: Arc<dyn ActiveCheck>) -> Self {
        Self::with_grace(inner, active, GRACE)
    }

    /// 확인을 기다리는 한도를 정해 만든다(시험에서 짧게 쓴다).
    pub fn with_grace(
        inner: Arc<dyn RemoteWire>,
        active: Arc<dyn ActiveCheck>,
        grace: Duration,
    ) -> Self {
        Self {
            inner,
            active,
            grace,
        }
    }

    /// 보내기 직전의 확인. 마감을 넘겨 도착한 성공은 받지 않는다(마감과 완료가 같은 때에 준비돼도 시각을 마감과 대조한다).
    async fn confirm(&self) -> Result<(), WireFail> {
        let limit = Instant::now() + self.grace;
        match timeout_at(limit, self.active.verify()).await {
            Ok(Ok(())) if Instant::now() < limit => Ok(()),
            _ => Err(WireFail::Stale),
        }
    }
}

impl RemoteWire for GuardedWire {
    fn send(&self, request: WireRequest) -> WireReply<'_> {
        Box::pin(async move {
            // 구판이면(또는 확인하지 못하면) 아무것도 보내지 않는다
            self.confirm().await?;
            self.inner.send(request).await
        })
    }
}

#[cfg(test)]
mod tests;
