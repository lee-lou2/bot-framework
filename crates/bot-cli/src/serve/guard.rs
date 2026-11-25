//! 구판 프로세스를 닫는 래퍼(워커용). 이 프로세스의 활성화 판본이 저장소의 활성 판본과 다르면(다른 프로세스가 모델 허용·
//! 상류 목적지·egress·설치 판본 같은 권한 설정을 바꿨다) 새 일을 집지 않고 도구를 시작하지 않는다 — 구판 워커가 철회된 호스트
//! 목적지를 쓰지 못하게 한다. 이미 집은 run은 끝까지 하게 두고(결과·체크포인트는 그대로 저장한다), 새 도구 호출만 막는다.
//! 모델 CLI는 예외다: 상류 목적지가 바로 호스트 설정이므로 이미 집은 시도라도 **상류로 나가는 모든 요청 직전**에 같은 판본인지 확인하고
//! 열린 스트림도 주기적으로 다시 확인해 끊는다(`GuardedUpstream`).
use bot_core::{
    command::{
        Accept, AcceptExtras, Beat, CallResult, Failure, Finish, Retry, RunOutcome, Step, ToolCall,
        Turn,
    },
    id::{ApprovalId, Attempt, PrincipalId, RunId},
    intake::{RequestKey, RequestState},
    port::{Broker, Error, Reply, RunStore},
    state::RunContext,
};
use bot_gateway::active::ActiveCheck;
use bot_store::Store;
use std::{sync::Arc, time::Duration};

/// 구판이 된 프로세스가 집은 run을 **시도 횟수를 쓰지 않고** 대기열로 돌려주는 저장소 쪽 포트. «이 핸들의 활성 집합이 더는 현재가 아니고 이 시도가 지금
/// 리스·세대의 소유자인가»는 저장소가 판본 교체와 같은 잠금 아래에서 판단한다.
pub trait Handover: Send + Sync {
    /// 실패 보고와 반납을 **한 DB 판정**으로: 구판이고 소유자면 대기열로(`Retry::After(0)`), 아니면 일반 실패의 결과. 판정을 읽지 못하면
    /// (`Unavailable`) 아무것도 기록하지 않았으니 같은 호출을 다시 해도 된다.
    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry>;

    /// 물러나는 run의 조건부 반납: 구판이고 아직 소유자일 때만 대기열로 돌아간다. 아니면 오류(`Denied`: 현재 판본이거나 취소·민감 run, `Stale`: 소유자가
    /// 아님).
    fn release(&self, attempt: Attempt) -> Reply<'_, ()>;
}

impl Handover for Store {
    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry> {
        Box::pin(async move { self.fail_or_release(&attempt, failure).await })
    }

    fn release(&self, attempt: Attempt) -> Reply<'_, ()> {
        Box::pin(async move { self.release_stale(&attempt).await })
    }
}

/// 워커가 run에서 물러날 때(`Flow::Lost`) 저장소에 조건부 반납을 청하는 입구(`bot_runtime::Handback`)를 `Handover`에 잇는다.
pub struct StoreHandback(pub Arc<dyn Handover>);

impl bot_runtime::Handback for StoreHandback {
    fn release(&self, attempt: Attempt) -> Reply<'_, ()> {
        self.0.release(attempt)
    }
}

/// 판정을 읽지 못했을 때(`Unavailable`) 같은 실패 보고를 다시 하는 횟수와 첫 쉼. 쉼은 시도마다 늘어난다.
const FAIL_TRIES: usize = 4;
const FAIL_PAUSE: Duration = Duration::from_millis(250);

/// `claim`만 확인하고 나머지는 그대로 위임하는 실행 저장소
pub struct GuardedRuns {
    inner: Arc<dyn RunStore>,
    active: Arc<dyn ActiveCheck>,
    handover: Option<Arc<dyn Handover>>,
    tries: usize,
    pause: Duration,
}

impl GuardedRuns {
    pub fn new(inner: Arc<dyn RunStore>, active: Arc<dyn ActiveCheck>) -> Self {
        Self {
            inner,
            active,
            handover: None,
            tries: FAIL_TRIES,
            pause: FAIL_PAUSE,
        }
    }

    /// 실패 보고(`fail`)를 저장소의 한 판정(구판이면 반납, 아니면 일반 실패)에 맡긴다.
    pub fn with_handover(mut self, handover: Arc<dyn Handover>) -> Self {
        self.handover = Some(handover);
        self
    }

    /// 판정을 읽지 못했을 때 다시 하는 횟수와 첫 쉼을 정한다(시험에서 짧게 쓴다).
    #[cfg(test)]
    pub fn with_retry(mut self, tries: usize, pause: Duration) -> Self {
        (self.tries, self.pause) = (tries.max(1), pause);
        self
    }
}

impl RunStore for GuardedRuns {
    fn accept(&self, command: Accept) -> Reply<'_, RunId> {
        self.inner.accept(command)
    }

    /// 인증된 원천·넘김 사슬을 덧붙인 접수도 그대로 넘긴다(기본 구현은 덧붙인 것을 버려, 빠지면 사슬이 조용히 사라진다).
    fn accept_with(&self, command: Accept, extras: AcceptExtras) -> Reply<'_, RunId> {
        self.inner.accept_with(command, extras)
    }

    fn status(&self, key: RequestKey) -> Reply<'_, Option<RequestState>> {
        self.inner.status(key)
    }

    /// 구판이면 새 일을 집지 않는다(`None` — 일은 저장소에 그대로 남아 새 판본의 워커가 집는다).
    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        Box::pin(async move {
            if self.active.verify().await.is_err() {
                tracing::warn!(
                    code = "stale_worker",
                    "활성 판본이 바뀌어 새 일을 집지 않아요 — 새 판본으로 다시 시작하세요"
                );
                return Ok(None);
            }
            self.inner.claim().await
        })
    }

    /// 종료 반납은 구판 검사 없이 그대로 넘긴다: 구판이 된 프로세스도 자기 claim을 돌려줘야 한다(기본 구현은 `Unavailable`이라 빠지면 조용히 반납이 안 된다).
    fn release_for_shutdown(&self, attempt: Attempt) -> Reply<'_, ()> {
        self.inner.release_for_shutdown(attempt)
    }

    fn renew(&self, attempt: Attempt) -> Reply<'_, Beat> {
        self.inner.renew(attempt)
    }

    fn checkpoint_batch(&self, attempt: Attempt, steps: Vec<Step>) -> Reply<'_, ()> {
        self.inner.checkpoint_batch(attempt, steps)
    }

    fn finish(&self, attempt: Attempt, done: Finish) -> Reply<'_, ()> {
        self.inner.finish(attempt, done)
    }

    /// 완료와 같은 커밋에 실행 계량(잘림·실측 사용량)을 싣는 완료도 그대로 넘긴다. 기본 구현은 계량을 버리고 `finish`로 끝내므로 빠지면 저장소가 계량을 영영 받지 못한다.
    fn finish_with(&self, attempt: Attempt, done: Finish, outcome: RunOutcome) -> Reply<'_, ()> {
        self.inner.finish_with(attempt, done, outcome)
    }

    /// 구판이 된 프로세스의 실패 보고는 run의 잘못이 아닐 수 있다(철회된 모델 목적지를 거절당한 모델 CLI 등). 반납과 일반 실패를 **저장소의 한 판정**으로 정한다:
    /// 이 핸들의 활성 집합이 더는 현재가 아니고 이 시도가 지금 소유자이면 시도 횟수를 쓰지 않고 대기열로 돌려 새 판본의 워커가 집게 하고(마지막 시도에서도
    /// dead letter가 되지 않는다), 아니면 보고한 실패 그대로 일반 실패다. 판정을 읽지 못하면(`Unavailable`: 활성 집합 행 잠금 시간 초과 등) **일반 실패를
    /// 기록하지 않고** 같은 호출을 몇 번 더 한다 — 그래도 안 되면 오류를 돌려 워커가 물러나게 한다(리스가 끝나면 다른 워커가 이어받는다).
    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry> {
        Box::pin(async move {
            let Some(handover) = &self.handover else {
                return self.inner.fail(attempt, failure).await;
            };
            let mut pause = self.pause;
            for left in (0..self.tries).rev() {
                match handover.fail(attempt.clone(), failure).await {
                    Ok(Retry::After(delay)) if delay.is_zero() => {
                        tracing::warn!(
                            code = "stale_worker",
                            "활성 판본이 바뀐 프로세스가 run을 대기열로 돌려줬어요 — 새 판본의 워커가 이어 가요"
                        );
                        return Ok(Retry::After(delay));
                    }
                    Err(Error::Unavailable) if left > 0 => {
                        tokio::time::sleep(pause).await;
                        pause += self.pause;
                    }
                    other => return other,
                }
            }
            Err(Error::Unavailable)
        })
    }

    fn history(&self, attempt: Attempt, limit: u32) -> Reply<'_, Vec<Turn>> {
        self.inner.history(attempt, limit)
    }
}

/// 도구 호출을 시작하기 전에 같은 판본인지 확인하는 브로커. 승인 결정은 그대로 위임한다(저장소가 판본에 묶어 지킨다).
pub struct GuardedBroker {
    inner: Arc<dyn Broker>,
    active: Arc<dyn ActiveCheck>,
}

impl GuardedBroker {
    pub fn new(inner: Arc<dyn Broker>, active: Arc<dyn ActiveCheck>) -> Self {
        Self { inner, active }
    }
}

impl Broker for GuardedBroker {
    fn call(&self, call: ToolCall) -> Reply<'_, CallResult> {
        Box::pin(async move {
            // 구판이면 도구를 시작하지 않는다(일시 실패 — 새 판본의 워커가 이어 간다)
            self.active.verify().await?;
            self.inner.call(call).await
        })
    }

    fn decide(&self, nonce: ApprovalId, actor: PrincipalId, allow: bool) -> Reply<'_, ()> {
        self.inner.decide(nonce, actor, allow)
    }
}

mod upstream;
mod wire;

pub use upstream::GuardedUpstream;
pub use wire::GuardedWire;

#[cfg(test)]
mod tests;
