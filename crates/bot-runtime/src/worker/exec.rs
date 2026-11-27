//! Single-run worker execution lifecycle: heartbeat, target dispatch, settlement, shutdown release, and gateway nudging.

use super::{Flow, Outcome, Worker, calls::Called, control::Control, interrupted, span::RunSpan};
use bot_core::{
    command::{Beat, Completion, Failure, Finish, Retry, RunOutcome, Target},
    pass::direct_call_id,
    port::Error,
    state::{ConversationKind, RunContext},
};
use std::time::Duration;
use tokio::sync::watch;

const NUDGE_TIMEOUT: Duration = Duration::from_secs(2);
const HAND_BACK_TIMEOUT: Duration = Duration::from_secs(5);

impl Worker {
    pub async fn execute(&self, ctx: RunContext) -> Outcome {
        self.execute_with(ctx, watch::channel(false).1).await
    }

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
        // If a frozen scheduled-delivery draft already exists on resume, skip re-running the model.
        let flow = match self.delivery(&ctx, None).await {
            Some(flow) => flow,
            None => match &ctx.work.target {
                Target::Tool { .. } => self.direct(&ctx, &control).await,
                Target::Agent { .. } => self.agent(&ctx, &control, &span).await,
            },
        };
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

    async fn direct(&self, ctx: &RunContext, control: &Control) -> Flow {
        let Target::Tool { tool, args, .. } = &ctx.work.target else {
            return Flow::Fail(Failure::Permanent("invalid_target"));
        };
        if let Some(flow) = interrupted(control) {
            return flow;
        }
        let call = direct_call_id(&ctx.attempt.run, ctx.work.pass_pos.unwrap_or(0));
        match self
            .call_tool(Some(control.released()), &ctx.attempt, &call, tool, args)
            .await
        {
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

    async fn settle(
        &self,
        ctx: &RunContext,
        control: &Control,
        flow: Flow,
    ) -> (Outcome, Option<&'static str>) {
        match flow {
            Flow::Parked => (Outcome::Parked, None),
            Flow::Lost => {
                // Store fences new work with `Error::Stale` on cancel requests too; distinguish an active cancel from true lease loss.
                if !control.lost() && self.cancel_pending(ctx).await {
                    return self
                        .report(ctx, control, Failure::Permanent("cancelled"))
                        .await;
                }
                self.hand_back(ctx).await;
                (Outcome::Lost, None)
            }
            Flow::Delivered => {
                self.nudge(ctx, true).await;
                (Outcome::Succeeded, None)
            }
            Flow::Release if control.lost() => (Outcome::Lost, None),
            Flow::Release => self.release(ctx).await,
            // Never write any outcome after losing the lease.
            _ if control.lost() => (Outcome::Lost, None),
            Flow::Done(finish, outcome) => match self
                .deps
                .runs
                .finish_with(ctx.attempt.clone(), finish, outcome)
                .await
            {
                Ok(Completion::Cancelled) => (Outcome::Cancelled, Some("cancelled")),
                Ok(Completion::Succeeded) => {
                    self.nudge(ctx, false).await;
                    (Outcome::Succeeded, None)
                }
                Err(Error::Stale | Error::Denied | Error::Missing) => (Outcome::Lost, None),
                Err(Error::Invalid) => {
                    self.report(ctx, control, Failure::Permanent("invalid_result"))
                        .await
                }
                Err(Error::Conflict) => {
                    self.report(ctx, control, Failure::Permanent("finish_conflict"))
                        .await
                }
                Err(Error::Unavailable | Error::RateLimited) => {
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
            Err(_) => (Outcome::Lost, Some(code)),
        }
    }

    async fn cancel_pending(&self, ctx: &RunContext) -> bool {
        matches!(
            tokio::time::timeout(HAND_BACK_TIMEOUT, self.deps.runs.renew(ctx.attempt.clone()))
                .await,
            Ok(Ok(Beat {
                cancel_requested: true
            }))
        )
    }

    async fn hand_back(&self, ctx: &RunContext) {
        if let Some(handback) = &self.handback {
            let _ = tokio::time::timeout(HAND_BACK_TIMEOUT, handback.release(ctx.attempt.clone()))
                .await;
        }
    }

    /// Confirms all model/tool subprocesses and session tokens are quiesced before releasing the attempt back to the queue.
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
