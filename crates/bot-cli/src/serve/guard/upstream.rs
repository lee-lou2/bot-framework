//! 모델 상류 전송의 판본 확인(`GuardedUpstream`). 구판 보호의 모델 쪽 절반이다(`guard.rs`의 실행 저장소·브로커 래퍼와 한 짝).
use bot_broker::model::{ChunkSource, Upstream, UpstreamRequest, UpstreamResponse, UpstreamStream};
use bot_core::port::{Error, Reply};
use bot_gateway::active::ActiveCheck;
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, sleep_until, timeout_at};

/// 열린 모델 스트림이 호스트 활성화 판본을 다시 확인하는 간격
const STREAM_CHECK: Duration = Duration::from_secs(1);
/// 확인하지 못하는 동안(저장소 불통·확인이 끝나지 않음) 열린 스트림을 이어 두는 한도이자 새 요청 직전 확인이 기다리는 한도. 넘기면 끊는다
/// (보이지 않는 채로 옛 목적지에 계속 흘리지 않는다).
const STREAM_GRACE: Duration = Duration::from_secs(10);

/// 모델 상류로 나가는 요청 직전에 같은 판본인지 확인하는 전송. 구판(`Stale`)이면 아무것도 보내지 않는다.
///
/// 오류는 `ModelProxy`/프런트가 이렇게 CLI에 알린다(실제 매핑): 요청을 시작할 때의 `Stale`(구판·미게시)은 403 `permission_error`라 CLI가
/// 재시도하지 않고, `Unavailable`(저장소를 읽지 못함·확인이 `grace`를 넘김)은 503 `overloaded_error`라 CLI가 다시 보낼 수 있다 — 어느 쪽이든 상류로
/// 나가는 것은 없다. 열린 스트림 도중의 오류는 둘 다 같은 고정 `api_error` 이벤트로 스트림을 끝내고(CLI가 다시 보내면 위 시작 확인을 지난다),
/// 열린 스트림은 `every`마다 다시 확인한다. 확인하지 못하는 구간은 **단일 마감**(처음 확인이 마땅했던 시각 + `grace`)으로 확인 자체와 재시도를
/// 묶는다: 그 안에 성공하지 못하면 끊고, 마감을 넘겨 도착한 성공은 받지 않으며, 마감 뒤에는 조각도 내보내지 않는다.
pub struct GuardedUpstream {
    inner: Arc<dyn Upstream>,
    active: Arc<dyn ActiveCheck>,
    every: Duration,
    grace: Duration,
}

impl GuardedUpstream {
    pub fn new(inner: Arc<dyn Upstream>, active: Arc<dyn ActiveCheck>) -> Self {
        Self::with_timing(inner, active, STREAM_CHECK, STREAM_GRACE)
    }

    /// 확인 간격과 유예를 정해 만든다(시험에서 짧게 쓴다).
    pub fn with_timing(
        inner: Arc<dyn Upstream>,
        active: Arc<dyn ActiveCheck>,
        every: Duration,
        grace: Duration,
    ) -> Self {
        Self {
            inner,
            active,
            every,
            grace,
        }
    }

    /// 새 요청을 보내기 직전의 확인. 끝나지 않는 확인이 요청을 붙잡아 두지 못하게 `grace`까지만 기다리고, 마감을 넘겨 도착한 성공은 받지 않는다
    /// (마감과 완료가 같은 때에 준비돼도 시각을 마감과 대조한다). 늦게 도착한 거절(`Stale`)은 그대로 받는다.
    async fn confirm(&self) -> Result<(), Error> {
        let limit = Instant::now() + self.grace;
        match timeout_at(limit, self.active.verify()).await {
            Ok(Ok(())) if Instant::now() < limit => Ok(()),
            Ok(Err(error)) => Err(error),
            _ => Err(Error::Unavailable),
        }
    }
}

impl Upstream for GuardedUpstream {
    fn send(&self, request: UpstreamRequest) -> Reply<'_, UpstreamResponse> {
        Box::pin(async move {
            // 구판이면(또는 확인하지 못하면) 아무것도 보내지 않는다
            self.confirm().await?;
            self.inner.send(request).await
        })
    }

    fn stream(&self, request: UpstreamRequest) -> Reply<'_, UpstreamStream> {
        Box::pin(async move {
            self.confirm().await?;
            let UpstreamStream { status, body } = self.inner.stream(request).await?;
            Ok(UpstreamStream {
                status,
                body: Box::new(Watched {
                    inner: body,
                    active: self.active.clone(),
                    every: self.every,
                    grace: self.grace,
                    next_check: Instant::now() + self.every,
                    deadline: None,
                }),
            })
        })
    }
}

/// 조각을 내주는 동안 주기적으로 판본을 다시 확인하는 본문
struct Watched {
    inner: Box<dyn ChunkSource>,
    active: Arc<dyn ActiveCheck>,
    every: Duration,
    grace: Duration,
    /// 다음 확인이 마땅한 시각
    next_check: Instant,
    /// 확인하지 못하는 구간의 단일 마감(그 구간에서 처음 확인이 마땅했던 시각 + `grace`). 확인에 성공하면 비운다.
    deadline: Option<Instant>,
}

impl ChunkSource for Watched {
    fn next(&mut self) -> Reply<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            let Self {
                inner,
                active,
                every,
                grace,
                next_check,
                deadline,
            } = self;
            // 읽기를 취소하지 않는다: 확인하는 동안에도 같은 읽기를 이어서 기다린다(조각을 잃지 않는다)
            let mut pending = inner.next();
            loop {
                // 마감을 넘기면 조각이 준비돼 있어도 내보내지 않는다
                if deadline.is_some_and(|limit| Instant::now() >= limit) {
                    return Err(Error::Unavailable);
                }
                let wake = deadline.map_or(*next_check, |limit| (*next_check).min(limit));
                // 확인할 때가 됐으면(또는 마감이 됐으면) 준비된 조각보다 먼저 처리한다 — 조각이 끊임없이 와도 확인이 밀리지 않는다
                tokio::select! {
                    biased;
                    () = sleep_until(wake) => {
                        if deadline.is_some_and(|limit| Instant::now() >= limit) {
                            return Err(Error::Unavailable);
                        }
                        if Instant::now() < *next_check {
                            continue;
                        }
                        // 이 구간의 단일 마감: 확인이 마땅했던 시각부터. 확인 자체와 그 재시도를 한 마감으로 묶는다
                        let limit = *deadline.get_or_insert(*next_check + *grace);
                        match timeout_at(limit, active.verify()).await {
                            // 마감 안의 성공만 받는다(마감을 넘겨 도착한 성공은 받지 않는다)
                            Ok(Ok(())) if Instant::now() < limit => {
                                *deadline = None;
                                *next_check = Instant::now() + *every;
                            }
                            // 구판: 늦게 도착한 거절도 그대로 받는다
                            Ok(Err(Error::Stale)) => return Err(Error::Stale),
                            // 확인하지 못했다(저장소 불통·시간 초과·늦은 성공): 마감 전에 한 번 더 본다
                            _ => *next_check = (Instant::now() + *every).min(limit),
                        }
                    }
                    chunk = &mut pending => {
                        // 마감과 조각이 함께 준비됐으면 마감이 먼저다(늦게 도착한 조각은 내보내지 않는다)
                        if deadline.is_some_and(|limit| Instant::now() >= limit) {
                            return Err(Error::Unavailable);
                        }
                        return chunk;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
