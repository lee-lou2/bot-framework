//! Provider fallback dispatch for agent runs, preserving zero-duplicate-effect safety across runners.

use super::{Flow, Worker, control::Control, interrupted, span::RunSpan};
use crate::{context::Assembled, model::RunnerKind};
use bot_core::{command::Failure, port::Error, profile::Snapshot, state::RunContext};
use std::sync::Arc;

fn failure_code(failure: &Failure) -> &'static str {
    match failure {
        Failure::Transient(code) | Failure::Permanent(code) => code,
    }
}

/// Non-provider or terminal workflow errors that must never trigger a model fallback.
fn non_fallback(code: &str) -> bool {
    matches!(
        code,
        "cancelled"
            | "too_many_turns"
            | "corrupt_checkpoint"
            | "checkpoint_conflict"
            | "invalid_checkpoint"
            | "invalid_chat"
            | "unknown_agent"
            | "invalid_target"
    )
}

impl Worker {
    /// Runs `assembled` across its primary alias and any configured fallback chain.
    ///
    /// Switching across `Loop` runners (or between `Loop` and `Step`) is allowed only when the ledger
    /// holds zero committed steps so an already-executed tool call can never be duplicated.
    pub(super) async fn run_with_fallback(
        &self,
        ctx: &RunContext,
        control: &Control,
        span: &RunSpan,
        snapshot: &Arc<Snapshot>,
        assembled: Assembled,
    ) -> Flow {
        let mut aliases = vec![assembled.model.clone()];
        aliases.extend(
            self.deps
                .models
                .fallbacks(&ctx.attempt.profile, &assembled.model),
        );
        let mut active_ctx = ctx.clone();
        for (index, alias) in aliases.iter().enumerate() {
            if let Some(flow) = interrupted(control) {
                return flow;
            }
            let Some(runner) = self.deps.models.runner(&active_ctx.attempt.profile, alias) else {
                return Flow::Fail(Failure::Permanent("unknown_model"));
            };
            let mut req_assembled = assembled.clone();
            req_assembled.model = alias.clone();
            let current_kind = runner.kind();
            let flow = match current_kind {
                RunnerKind::Loop => {
                    self.loop_run(
                        &active_ctx,
                        control,
                        span,
                        snapshot,
                        runner.as_ref(),
                        req_assembled,
                        index,
                    )
                    .await
                }
                RunnerKind::Step => {
                    self.step_run(
                        &active_ctx,
                        control,
                        span,
                        snapshot,
                        runner.as_ref(),
                        req_assembled,
                    )
                    .await
                }
            };
            let Flow::Fail(failure) = flow else {
                return flow;
            };
            let code = failure_code(&failure);
            let Some(next_alias) = aliases.get(index + 1) else {
                return Flow::Fail(failure);
            };
            if control.stopped() || non_fallback(code) {
                return Flow::Fail(failure);
            }
            let Some(next_runner) = self
                .deps
                .models
                .runner(&active_ctx.attempt.profile, next_alias)
            else {
                return Flow::Fail(failure);
            };
            let latest = match self.deps.ledger.context(active_ctx.attempt.clone()).await {
                Ok(latest) => latest,
                Err(Error::Stale | Error::Denied | Error::Missing) => return Flow::Lost,
                Err(_) => return Flow::Fail(failure),
            };
            let step_to_step =
                current_kind == RunnerKind::Step && next_runner.kind() == RunnerKind::Step;
            if !step_to_step && !latest.work.steps.is_empty() {
                return Flow::Fail(failure);
            }
            tracing::warn!(
                profile = %active_ctx.attempt.profile.0,
                run = %active_ctx.attempt.run.0,
                from = %alias,
                to = %next_alias,
                code,
                "model failed; falling back to next provider"
            );
            active_ctx = latest;
        }
        Flow::Fail(Failure::Permanent("unknown_model"))
    }
}
