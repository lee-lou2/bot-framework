//! run 한 건의 실행: 하트비트를 켜고, 대상(도구 직접 호출·에이전트)에 맞게 돌린 뒤, 결과를 저장소에 보고한다.
//! 리스를 잃은 뒤에는 어떤 결과도 쓰지 않는다(옛 세대의 보고가 새 소유자를 덮지 못한다).

use super::{Flow, Outcome, Worker, calls::Called, control::Control, interrupted, span::RunSpan};
use bot_core::{
    command::{Beat, Failure, Finish, Retry, RunOutcome, Target},
    id::CallId,
    port::Error,
    state::{ConversationKind, RunContext},
};
use std::time::Duration;
use tokio::sync::watch;

/// 결과를 커밋한 뒤 게이트웨이를 깨우는 데 쓰는 시간 상한(깨우기는 최선이다)
const NUDGE_TIMEOUT: Duration = Duration::from_secs(2);
/// 물러나기 전에 조건부 반납을 청하는 데 쓰는 시간 상한(반납은 최선이다 — 못 하면 리스가 끝난 뒤 다른 워커가 이어받는다)
const HAND_BACK_TIMEOUT: Duration = Duration::from_secs(5);

impl Worker {
    /// 집은 run 하나를 끝까지(또는 멈출 때까지) 실행한다.
    pub async fn execute(&self, ctx: RunContext) -> Outcome {
        // 송신 쪽이 없는 수신기: 종료 반납 신호가 올 일이 없다(시험·단발 실행)
        self.execute_with(ctx, watch::channel(false).1).await
    }

    /// `execute`에 종료 반납 신호를 더한 것: `Worker::run`이 유예가 지난 뒤에도 남은 run에 올린다. 신호가 오면 실행을 멈추고 정리를 확인한 뒤 대기열로 돌려준다.
    pub(super) async fn execute_with(
        &self,
        ctx: RunContext,
        release: watch::Receiver<bool>,
    ) -> Outcome {
        let control = Control::start(
            self.deps.runs.clone(),
            ctx.attempt.clone(),
            self.cfg.heartbeat,
            self.cfg.heartbeat_misses,
            release,
        );
        let span = RunSpan::begin(self.deps.tracer.clone(), &ctx);
        // 이미 동결한 회신 초안이 있으면(승인·거절 뒤 재개) 모델을 다시 돌리지 않고 그 초안으로 끝낸다
        let flow = match self.delivery(&ctx, None).await {
            Some(flow) => flow,
            None => match &ctx.work.target {
                Target::Tool { .. } => self.direct(&ctx, &control).await,
                Target::Agent { .. } => self.agent(&ctx, &control, &span).await,
            },
        };
        // 방금 만든 최종 답이 다른 수신자로 가야 하면 일반 완료 대신 그 초안을 동결해 승인을 받는다
        let flow = match flow {
            Flow::Done(finish, outcome) => match self.delivery(&ctx, Some(finish.clone())).await {
                Some(flow) => flow,
                None => Flow::Done(finish, outcome),
            },
            other => other,
        };
        let (outcome, error) = self.settle(&ctx, &control, flow).await;
        span.end(&outcome, error);
        outcome
    }

    /// 도구를 직접 부르는 run(예약·이벤트 트리거의 도구 대상). 호출 ID는 run마다 하나로 고정이라 재개해도 같은 호출이다.
    async fn direct(&self, ctx: &RunContext, control: &Control) -> Flow {
        let Target::Tool { tool, args, .. } = &ctx.work.target else {
            return Flow::Fail(Failure::Permanent("invalid_target"));
        };
        if let Some(flow) = interrupted(control) {
            return flow;
        }
        let call = CallId(format!("{}:tool", ctx.attempt.run.0));
        match self
            .call_tool(Some(control.released()), &ctx.attempt, &call, tool, args)
            .await
        {
            // 결과는 run에 기록한다. 사용자 회신은 도구가 선언한 목적지(브로커)가 맡는다
            Ok(Called::Done(result)) => Flow::Done(
                Finish {
                    result,
                    answers: Vec::new(),
                },
                RunOutcome::default(),
            ),
            Ok(Called::Parked) => Flow::Parked,
            Err(error) => error.into_flow(),
        }
    }

    /// 흐름을 저장소에 보고하고 이 워커가 run을 어떻게 끝냈는지 돌려준다.
    async fn settle(
        &self,
        ctx: &RunContext,
        control: &Control,
        flow: Flow,
    ) -> (Outcome, Option<&'static str>) {
        match flow {
            Flow::Parked => (Outcome::Parked, None),
            Flow::Lost => {
                // 저장소는 취소 요청이 선 run의 새 일(읽기·체크포인트·도구 시작)을 `Stale`로 거절한다(`fence`). 워커는 이를 리스 상실과 구별하지 못해 `Lost`로 보지만,
                // 이 시도가 아직 소유자이고 운영자가 취소를 요청한 것이라면 잃은 것이 아니다: 취소로 끝낸다. 그대로 물러나면 리스가 끝나고 다른 워커가 집을 때까지(최대 한 리스)
                // 취소가 늦는다. 하트비트가 이미 리스 상실을 알았으면 확인하지 않는다
                if !control.lost() && self.cancel_pending(ctx).await {
                    return self
                        .report(ctx, control, Failure::Permanent("cancelled"))
                        .await;
                }
                self.hand_back(ctx).await;
                (Outcome::Lost, None)
            }
            // 저장소가 이미 등록과 완료를 한 커밋으로 마쳤다: 목적지 회신을 서둘러 내보낸다
            Flow::Delivered => {
                self.nudge(ctx, true).await;
                (Outcome::Succeeded, None)
            }
            // 종료 반납: 정리를 확인한 뒤 시도 횟수를 쓰지 않고 대기열로 돌려준다(리스를 이미 잃었으면 아무것도 쓰지 않는다)
            Flow::Release if control.lost() => (Outcome::Lost, None),
            Flow::Release => self.release(ctx).await,
            // 리스를 잃었다: 어떤 결과도 쓰지 않는다
            _ if control.lost() => (Outcome::Lost, None),
            Flow::Done(finish, outcome) => match self
                .deps
                .runs
                .finish_with(ctx.attempt.clone(), finish, outcome)
                .await
            {
                Ok(()) => {
                    self.nudge(ctx, false).await;
                    (Outcome::Succeeded, None)
                }
                Err(Error::Stale | Error::Denied | Error::Missing) => (Outcome::Lost, None),
                Err(Error::Invalid) => {
                    self.report(ctx, control, Failure::Permanent("invalid_result"))
                        .await
                }
                Err(_) => {
                    self.report(ctx, control, Failure::Transient("finish_unavailable"))
                        .await
                }
            },
            Flow::Fail(failure) => self.report(ctx, control, failure).await,
        }
    }

    async fn report(
        &self,
        ctx: &RunContext,
        control: &Control,
        failure: Failure,
    ) -> (Outcome, Option<&'static str>) {
        let code = match failure {
            Failure::Transient(code) | Failure::Permanent(code) => code,
        };
        match self.deps.runs.fail(ctx.attempt.clone(), failure).await {
            Ok(Retry::Dead) if control.cancelled() || code == "cancelled" => {
                (Outcome::Cancelled, Some(code))
            }
            Ok(retry) => (Outcome::Failed(retry), Some(code)),
            // 보고하지 못했다: 리스가 끝나면 다른 워커가 이어받는다
            Err(_) => (Outcome::Lost, Some(code)),
        }
    }

    /// 이 시도가 아직 소유자이고 운영자가 취소를 요청했는가: 하트비트와 같은 `renew`를 한 번 더 불러 확인한다(소유자가 아니면 오류라 `false`다).
    async fn cancel_pending(&self, ctx: &RunContext) -> bool {
        matches!(
            tokio::time::timeout(HAND_BACK_TIMEOUT, self.deps.runs.renew(ctx.attempt.clone()))
                .await,
            Ok(Ok(Beat {
                cancel_requested: true
            }))
        )
    }

    /// 물러나기 전에 저장소에 조건부 반납을 청한다: 이 프로세스의 활성 집합이 더는 현재가 아니고 이 시도가 아직 소유자일 때만 대기열로 돌아가
    /// (리스가 끝나기를 기다리다 인계되며 시도를 쓰지 않는다), 이미 인계됐거나 현재 판본이면 저장소가 거절한다. 결과는 보지 않는다.
    async fn hand_back(&self, ctx: &RunContext) {
        if let Some(handback) = &self.handback {
            let _ = tokio::time::timeout(HAND_BACK_TIMEOUT, handback.release(ctx.attempt.clone()))
                .await;
        }
    }

    /// 종료 반납: (1) 이 시도의 모델·도구 프로세스가 끝났고 세션 토큰이 거두어졌음을 확인한다 (2) 그 뒤에만 시도 횟수를 쓰지 않고 대기열로 돌려준다.
    /// 확인하지 못했거나(오류·시간 초과·확인 입구 없음) 반납을 거절당하면 아무것도 쓰지 않고 물러난다 — 리스가 끝나면 다른 워커가 인계 규칙으로 이어받는다.
    async fn release(&self, ctx: &RunContext) -> (Outcome, Option<&'static str>) {
        let Some(quiesce) = &self.quiesce else {
            return (Outcome::Lost, Some("shutdown_unconfirmed"));
        };
        let confirmed =
            tokio::time::timeout(self.cfg.cleanup / 2, quiesce.quiesce(ctx.attempt.clone())).await;
        if !matches!(confirmed, Ok(Ok(()))) {
            return (Outcome::Lost, Some("shutdown_unconfirmed"));
        }
        let released = tokio::time::timeout(
            self.cfg.cleanup / 4,
            self.deps.runs.release_for_shutdown(ctx.attempt.clone()),
        )
        .await;
        match released {
            Ok(Ok(())) => (Outcome::Released, None),
            _ => (Outcome::Lost, Some("shutdown_release_failed")),
        }
    }

    /// 푸시 대화의 루트 run 결과를 커밋했으면 게이트웨이를 깨워 회신을 서두른다(없어도 게이트웨이가 outbox를 스스로 점유한다).
    /// `to_destination`이면 원래 대화와 무관하게 목적지로 쌓인 회신이 있다.
    async fn nudge(&self, ctx: &RunContext, to_destination: bool) {
        let Some(gateway) = &self.deps.gateway else {
            return;
        };
        let pushed = to_destination
            || matches!(
                ctx.conversation.kind,
                ConversationKind::Direct | ConversationKind::Channel
            );
        if !pushed || ctx.root != ctx.attempt.run {
            return;
        }
        let deliver = gateway.deliver(ctx.attempt.run.clone(), ctx.attempt.profile.clone());
        let _ = tokio::time::timeout(NUDGE_TIMEOUT, deliver).await;
    }
}
