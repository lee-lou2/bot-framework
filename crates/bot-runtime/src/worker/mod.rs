//! 워커: 저장소에서 run을 집어 끝까지(또는 멈출 때까지) 실행한다.
//!
//! - 집기: `RunStore::claim`(SKIP LOCKED·리스·세대는 저장소 몫). 동시 실행 수는 `WorkerConfig::concurrency`로 제한한다.
//! - 리스: run마다 heartbeat(`renew`)로 늘린다. 취소 요청은 실행기에 중단 신호로 전하고, 리스를 잃으면(옛 세대·만료·승인 대기로 멈춤·
//!   저장소 불통이 이어짐) 멈추고 아무것도 쓰지 않는다(`control.rs`).
//! - 실행: 도구를 직접 부르는 run과 에이전트 run(`exec.rs`·`agent.rs`). 도구 호출은 `Broker::call`, 위임은 `DelegationStore::delegate`.
//! - 보고: 성공은 `finish`, 실패는 `fail`(고정 어휘 코드만). 승인 대기·보류·위임은 저장소가 run을 멈추므로 보고하지 않는다.
//! - 종료: 종료 신호가 오면 새 run을 집지 않고 진행 중인 run을 `grace`만큼 기다린다(하트비트는 유지한다). 그래도 남은 run은 모델·도구 프로세스를 끝내고
//!   `Quiesce`로 종료·세션 회수를 확인한 뒤 `RunStore::release_for_shutdown`으로 대기열에 돌려준다(`cleanup` 안에서). 확인하지 못했거나 반납을 거절당하면
//!   아무것도 쓰지 않고 놓는다 — 리스가 끝나면 다른 워커가 인계 규칙으로 이어받는다.

mod agent;
mod calls;
mod checkpoint;
mod control;
mod delivery;
mod exec;
mod session;
mod span;
#[cfg(test)]
mod tests;

pub use delivery::Delivery;

use crate::host::{Deps, Handback, Quiesce, WorkerConfig};
use bot_core::command::{Failure, Finish, Retry, RunOutcome};
use std::sync::Arc;
use tokio::{sync::watch, task::JoinSet};

/// 한 run을 이 워커가 어떻게 끝냈는가.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// 결과를 기록했다.
    Succeeded,
    /// 승인·보류·자식을 기다리느라 저장소가 run을 멈췄다(리스를 놓았다). 풀리면 이어서 실행된다.
    Parked,
    /// 실패를 보고했다(재시도 대기 또는 dead letter).
    Failed(Retry),
    /// 취소 요청을 받아 취소로 끝냈다.
    Cancelled,
    /// 리스를 잃어 아무것도 쓰지 않고 물러났다(다른 워커가 이어받는다).
    Lost,
    /// 종료 신호로 정리(프로세스 종료·세션 회수)를 확인하고 시도 횟수를 쓰지 않고 대기열로 돌려줬다. 다른 워커가 이어받는다.
    Released,
}

/// run 실행의 중간 결말(보고 전).
pub(crate) enum Flow {
    /// 결과와, 같은 커밋에 기록할 실행 계량(잘림·실측 사용량)
    Done(Finish, RunOutcome),
    /// 목적지가 다른 예약 회신을 소유자가 승인해 저장소가 등록과 run 완료를 한 커밋으로 마쳤다(워커가 `finish`로 다시 끝내지 않는다).
    Delivered,
    Parked,
    Fail(Failure),
    Lost,
    /// 종료 반납: 실행을 멈췄고 정리를 확인한 뒤 대기열로 돌려준다(`exec.rs`).
    Release,
}

/// 실행 중단 신호를 흐름으로 옮긴다: 리스를 잃었으면 물러나고, 취소 요청이면 취소로 끝낸다. 종료 반납 신호만이면 정리하고 돌려준다(취소가 먼저다).
pub(crate) fn interrupted(control: &control::Control) -> Option<Flow> {
    control.stopped().then(|| {
        if control.lost() {
            Flow::Lost
        } else if !control.cancelled() && control.shutdown() {
            Flow::Release
        } else {
            Flow::Fail(Failure::Permanent("cancelled"))
        }
    })
}

#[derive(Clone)]
pub struct Worker {
    pub(crate) deps: Arc<Deps>,
    pub(crate) cfg: Arc<WorkerConfig>,
    /// 목적지가 다른 예약 회신의 초안·승인 입구(없으면 그런 회신을 만들 수 없고 모든 예약은 원래 대화로 끝난다).
    pub(crate) delivery: Option<Delivery>,
    /// 물러나는 run을 저장소에 조건부로 돌려주는 입구(없으면 물러나기만 한다).
    pub(crate) handback: Option<Arc<dyn Handback>>,
    /// 종료 반납 직전에 이 시도의 모델·도구 프로세스 종료와 세션 회수를 확인하는 입구(없으면 종료 반납을 하지 않는다).
    pub(crate) quiesce: Option<Arc<dyn Quiesce>>,
}

impl Worker {
    pub fn new(deps: Deps, cfg: WorkerConfig) -> Self {
        Self {
            deps: Arc::new(deps),
            cfg: Arc::new(cfg),
            delivery: None,
            handback: None,
            quiesce: None,
        }
    }

    /// 목적지가 다른 예약 회신을 승인 절차로 끝내는 입구를 붙인다(호스트가 저장소의 `ScheduledReplies`와 브로커의 `ScheduleDelivery`를 함께 준다).
    #[must_use]
    pub fn with_delivery(mut self, delivery: Delivery) -> Self {
        self.delivery = Some(delivery);
        self
    }

    /// 물러나는(`Lost`) run을 저장소에 조건부로 돌려주는 입구를 붙인다(호스트가 구판 반납 함수에 잇는다).
    #[must_use]
    pub fn with_handback(mut self, handback: Arc<dyn Handback>) -> Self {
        self.handback = Some(handback);
        self
    }

    /// 종료 반납의 정리 확인 입구를 붙인다(호스트가 세션 회수·모델 샌드박스·도구 실행기의 `reap`에 잇는다).
    #[must_use]
    pub fn with_quiesce(mut self, quiesce: Arc<dyn Quiesce>) -> Self {
        self.quiesce = Some(quiesce);
        self
    }

    /// 한 건만 집어 끝까지(또는 멈출 때까지) 실행한다(시험·단발 실행). 집을 것이 없거나 저장소가 응답하지 못하면 `None`.
    pub async fn run_once(&self) -> Option<Outcome> {
        match self.deps.runs.claim().await {
            Ok(Some(ctx)) => Some(self.execute(ctx).await),
            Ok(None) => None,
            Err(_) => {
                tracing::warn!(code = "claim_unavailable", "worker claim failed");
                None
            }
        }
    }

    /// 종료 신호(`true`)가 올 때까지 run을 집어 실행한다. 반환하기 전에 진행 중인 run을 `grace`만큼 기다린다.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        let limit = self.cfg.concurrency.max(1);
        let mut tasks: JoinSet<Outcome> = JoinSet::new();
        // 유예가 지나도 끝나지 않은 run에 «정리하고 대기열로 돌려줘라»를 알리는 신호
        let (release, release_rx) = watch::channel(false);
        'serve: loop {
            if *shutdown.borrow() {
                break;
            }
            while let Some(finished) = tasks.try_join_next() {
                log_finished(finished);
            }
            if tasks.len() < limit {
                match self.deps.runs.claim().await {
                    Ok(Some(ctx)) => {
                        let worker = self.clone();
                        let release = release_rx.clone();
                        tasks.spawn(async move { worker.execute_with(ctx, release).await });
                        continue; // 한도까지 바로 이어서 집는다
                    }
                    Ok(None) => {}
                    Err(_) => tracing::warn!(code = "claim_unavailable", "worker claim failed"),
                }
            }
            // 쉰다: 종료 신호·작업 종료·poll 간격 중 먼저 오는 것
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        break 'serve;
                    }
                }
                Some(finished) = tasks.join_next(), if !tasks.is_empty() => log_finished(finished),
                () = tokio::time::sleep(self.cfg.poll) => {}
            }
        }
        let drain = async {
            while let Some(finished) = tasks.join_next().await {
                log_finished(finished);
            }
        };
        if tokio::time::timeout(self.cfg.grace, drain).await.is_err() {
            tracing::warn!(
                code = "grace_exceeded",
                "worker stopped with runs in flight"
            );
            // 남은 run을 정리한다: 모델·도구 프로세스를 끝내고 확인한 뒤 대기열로 돌려준다. 정리 시간이 끝나기 전에는 abort하지 않는다.
            let _ = release.send(true);
            let cleanup = async {
                while let Some(finished) = tasks.join_next().await {
                    log_finished(finished);
                }
            };
            if tokio::time::timeout(self.cfg.cleanup, cleanup)
                .await
                .is_err()
            {
                tracing::warn!(
                    code = "cleanup_exceeded",
                    "worker stopped with unfinished cleanup"
                );
            }
        }
        tasks.abort_all();
    }
}

fn log_finished(finished: Result<Outcome, tokio::task::JoinError>) {
    if let Err(error) = finished
        && error.is_panic()
    {
        tracing::error!(code = "run_panicked", "worker run panicked");
    }
}
