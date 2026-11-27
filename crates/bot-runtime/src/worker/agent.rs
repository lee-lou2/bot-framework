//! Agent-target run execution: context assembly, `Loop` and checkpointed `Step` model turns, and answer packing.

use super::{
    Flow, Worker,
    calls::Ran,
    checkpoint::{self, Planned, TurnUsage},
    control::Control,
    interrupted,
    session::RunSession,
    span::RunSpan,
};
use crate::{
    context::{self, Assembled, ContextError, ContextInput},
    model::{
        DoneCall, Exchange, ModelError, ModelMeta, ModelRunner, ModelStep, RunnerKind, Session,
    },
};
use bot_core::{
    attachments,
    command::{Failure, Finish, JsonPayload, RunOutcome, Step, TokenUsage},
    id::Generation,
    port::Error,
    profile::Snapshot,
    state::{ConversationKind, RunContext},
    trace::{SpanKind, key, now_nano},
};
use serde_json::json;
use std::{collections::HashSet, sync::Arc, time::SystemTime};

const MAX_ANSWER_CHARS: usize = 12_000;
/// Leaves room for the primary reply within the store's 16-answer cap.
const MAX_ATTACHMENT_ANSWERS: usize = 15;
const MAX_STATE_BYTES: usize = 256 * 1024;

/// Only root push conversations (Slack channel/DM) split inline `<attachment>` blocks; child runs and CLI/HTTP preserve raw text.
fn splits_attachments(ctx: &RunContext) -> bool {
    ctx.root == ctx.attempt.run
        && matches!(
            ctx.conversation.kind,
            ConversationKind::Channel | ConversationKind::Direct
        )
}

fn answer(ctx: &RunContext, text: &str, mut outcome: RunOutcome, usage_whole: bool) -> Flow {
    let text = text.trim();
    if text.is_empty() {
        return Flow::Fail(Failure::Permanent("empty_answer"));
    }
    let (body, files) = if splits_attachments(ctx) {
        attachments::split(text)
    } else {
        (text.to_owned(), Vec::new())
    };
    let body = body.trim();
    if body.is_empty() {
        return Flow::Fail(Failure::Permanent("empty_answer"));
    }
    let clipped: String = body.chars().take(MAX_ANSWER_CHARS).collect();
    outcome.truncated |= clipped.len() < body.len();
    let (extra, extra_truncated) = attachments::pack_answers(&files, MAX_ATTACHMENT_ANSWERS);
    outcome.truncated |= extra_truncated;
    if !usage_whole {
        outcome.usage = None;
    }
    let primary = JsonPayload(json!({ "text": clipped }).to_string());
    let mut answers = Vec::with_capacity(1 + extra.len());
    answers.push(primary.clone());
    answers.extend(extra);
    Flow::Done(
        Finish {
            result: primary,
            answers,
        },
        outcome,
    )
}

impl Worker {
    pub(super) async fn agent(&self, ctx: &RunContext, control: &Control, span: &RunSpan) -> Flow {
        let attempt = &ctx.attempt;
        let Some(snapshot) = self
            .deps
            .catalog
            .lookup(&attempt.profile, &ctx.work.revision)
        else {
            return Flow::Fail(Failure::Transient("unknown_revision"));
        };
        if let Some(flow) = interrupted(control) {
            return flow;
        }
        // Untrusted API chat roots carry the full caller-supplied conversation; never mix server history.
        let history = if context::is_chat(ctx) {
            Vec::new()
        } else {
            match self
                .deps
                .runs
                .history(attempt.clone(), self.cfg.history_turns)
                .await
            {
                Ok(turns) => turns,
                Err(Error::Stale | Error::Denied | Error::Missing) => return Flow::Lost,
                Err(_) => return Flow::Fail(Failure::Transient("history_unavailable")),
            }
        };
        let query = context::query_text(ctx);
        let memory = match self.deps.memory.search(attempt.clone(), query).await {
            Ok(hits) => hits,
            Err(Error::Stale | Error::Denied | Error::Missing) => return Flow::Lost,
            Err(_) => Vec::new(),
        };
        let assembled = match context::assemble(&ContextInput {
            snapshot: &snapshot,
            run: ctx,
            history: &history,
            memory: &memory,
            now: SystemTime::now(),
        }) {
            Ok(assembled) => assembled,
            Err(ContextError::UnknownAgent(_)) => {
                return Flow::Fail(Failure::Permanent("unknown_agent"));
            }
            Err(ContextError::NotAnAgentRun) => {
                return Flow::Fail(Failure::Permanent("invalid_target"));
            }
            Err(ContextError::InvalidChat) => {
                return Flow::Fail(Failure::Permanent("invalid_chat"));
            }
        };
        let Some(runner) = self.deps.models.runner(&attempt.profile, &assembled.model) else {
            return Flow::Fail(Failure::Permanent("unknown_model"));
        };
        match runner.kind() {
            RunnerKind::Loop => {
                self.loop_run(ctx, control, span, &snapshot, runner.as_ref(), assembled)
                    .await
            }
            RunnerKind::Step => {
                self.step_run(ctx, control, span, &snapshot, runner.as_ref(), assembled)
                    .await
            }
        }
    }

    async fn loop_run(
        &self,
        ctx: &RunContext,
        control: &Control,
        span: &RunSpan,
        snapshot: &Arc<Snapshot>,
        runner: &dyn ModelRunner,
        assembled: Assembled,
    ) -> Flow {
        let alias = assembled.model.clone();
        // Dropping `grant` when this function exits revokes only this attempt's relay token.
        let grant = self
            .deps
            .relays
            .open(&ctx.attempt, &ctx.agent, &assembled.model);
        let session = grant.as_ref().map(|g| {
            Session(Arc::new(RunSession::new(
                self.clone(),
                ctx.clone(),
                snapshot.clone(),
                &g.channel.name,
            )))
        });
        let request = assembled.into_request(
            ctx.attempt.clone(),
            Vec::new(),
            self.cfg.limits,
            control.signal(),
            grant.as_ref().map(|g| g.channel.clone()),
            session,
        );
        let started = now_nano();
        let meta = runner.meta(&alias);
        let result = runner.step(request).await;
        self.model_span(span, 0, &alias, meta.as_ref(), started, &result);
        match result {
            Ok(ModelStep::Final { text, outcome, .. }) => {
                answer(ctx, &text, outcome, ctx.attempt.generation == Generation(1))
            }
            Ok(ModelStep::Calls { .. }) => Flow::Fail(Failure::Permanent("invalid_model_output")),
            Err(error) => model_error(control, error),
        }
    }

    async fn step_run(
        &self,
        ctx: &RunContext,
        control: &Control,
        span: &RunSpan,
        snapshot: &Snapshot,
        runner: &dyn ModelRunner,
        assembled: Assembled,
    ) -> Flow {
        let Some(restored) = checkpoint::restore(&ctx.work.steps) else {
            return Flow::Fail(Failure::Permanent("corrupt_checkpoint"));
        };
        let mut exchanges = restored.done;
        let mut planned = restored.planned;
        let mut usages = restored.usages;
        loop {
            if let Some(flow) = interrupted(control) {
                return flow;
            }
            let turn = exchanges.len();
            if turn >= self.cfg.max_turns {
                return Flow::Fail(Failure::Permanent("too_many_turns"));
            }
            let plan = match planned.take() {
                Some(plan) => plan,
                None => {
                    let request = assembled.clone().into_request(
                        ctx.attempt.clone(),
                        exchanges.clone(),
                        self.cfg.limits,
                        control.signal(),
                        None,
                        None,
                    );
                    let started = now_nano();
                    let meta = runner.meta(&assembled.model);
                    let result = runner.step(request).await;
                    self.model_span(
                        span,
                        turn,
                        &assembled.model,
                        meta.as_ref(),
                        started,
                        &result,
                    );
                    match result {
                        Ok(ModelStep::Final { text, outcome, .. }) => {
                            let total = step_usage(
                                &usages,
                                exchanges.len(),
                                has_children(ctx, &exchanges),
                                outcome.usage,
                                ctx.attempt.generation == Generation(1),
                            );
                            return answer(
                                ctx,
                                &text,
                                RunOutcome {
                                    usage: total,
                                    ..outcome
                                },
                                true,
                            );
                        }
                        Ok(ModelStep::Calls {
                            text,
                            calls,
                            measured,
                            state,
                            ..
                        }) => {
                            let ids: HashSet<&str> =
                                calls.iter().map(|c| c.call.0.as_str()).collect();
                            if calls.is_empty() || ids.len() != calls.len() || ids.contains("") {
                                return Flow::Fail(Failure::Permanent("invalid_model_output"));
                            }
                            if state.as_ref().is_some_and(|s| s.len() > MAX_STATE_BYTES) {
                                return Flow::Fail(Failure::Permanent("invalid_model_output"));
                            }
                            let usage = measured.map_or(TurnUsage::Unreported, TurnUsage::Reported);
                            let plan = Planned {
                                text,
                                calls,
                                usage,
                                state,
                            };
                            // Checkpoint the plan before running calls so retries reuse identical call IDs.
                            if let Err(flow) = self
                                .checkpoint(
                                    ctx,
                                    Step {
                                        key: checkpoint::plan_key(turn),
                                        body: plan.to_payload(),
                                    },
                                )
                                .await
                            {
                                return flow;
                            }
                            usages.push(usage);
                            plan
                        }
                        Err(error) => return model_error(control, error),
                    }
                }
            };
            let mut done = Vec::with_capacity(plan.calls.len());
            for call in &plan.calls {
                if let Some(flow) = interrupted(control) {
                    return flow;
                }
                let end = match self
                    .run_planned_call(ctx, Some(control.released()), snapshot, span, call)
                    .await
                {
                    Ok(Ran::End(end)) => end,
                    Ok(Ran::Refused(reason)) => crate::model::CallEnd::Failed(reason.word().into()),
                    Ok(Ran::Parked) => return Flow::Parked,
                    Err(flow) => return flow,
                };
                done.push(DoneCall {
                    call: call.call.clone(),
                    tool: call.tool.clone(),
                    args: call.args.clone(),
                    end,
                });
            }
            let exchange = Exchange {
                text: plan.text,
                calls: done,
                state: plan.state,
            };
            if let Err(flow) = self
                .checkpoint(
                    ctx,
                    Step {
                        key: checkpoint::end_key(turn),
                        body: exchange.to_payload(),
                    },
                )
                .await
            {
                return flow;
            }
            exchanges.push(exchange);
        }
    }

    pub(super) async fn checkpoint(&self, ctx: &RunContext, step: Step) -> Result<(), Flow> {
        match self.deps.runs.checkpoint(ctx.attempt.clone(), step).await {
            Ok(()) => Ok(()),
            Err(Error::Stale | Error::Denied | Error::Missing) => Err(Flow::Lost),
            Err(Error::Conflict) => Err(Flow::Fail(Failure::Permanent("checkpoint_conflict"))),
            Err(Error::Invalid) => Err(Flow::Fail(Failure::Permanent("invalid_checkpoint"))),
            Err(Error::Unavailable | Error::RateLimited) => {
                Err(Flow::Fail(Failure::Transient("checkpoint_unavailable")))
            }
        }
    }

    fn model_span(
        &self,
        span: &RunSpan,
        turn: usize,
        alias: &str,
        meta: Option<&ModelMeta>,
        started: u64,
        result: &Result<ModelStep, ModelError>,
    ) {
        let mut record = span
            .child("model", &turn.to_string(), "worker.model")
            .kind(SpanKind::Client)
            .started_at(started)
            .attr(key::GEN_AI_OPERATION_NAME, "chat")
            .attr(key::GEN_AI_REQUEST_MODEL, alias.to_owned());
        if let Some(meta) = meta {
            record = record.attr(key::GEN_AI_PROVIDER_NAME, meta.provider);
            if result.is_ok() {
                record = record.attr(key::GEN_AI_RESPONSE_MODEL, meta.model.clone());
            }
        }
        match result {
            Ok(ModelStep::Final { usage, .. } | ModelStep::Calls { usage, .. }) => {
                if usage.input_tokens > 0 {
                    record = record.attr(key::GEN_AI_USAGE_INPUT_TOKENS, usage.input_tokens);
                }
                if usage.output_tokens > 0 {
                    record = record.attr(key::GEN_AI_USAGE_OUTPUT_TOKENS, usage.output_tokens);
                }
            }
            Err(error) => record = record.fail(error.code()),
        }
        span.emit(record);
    }
}

/// True only when a delegation call actually spawned a child (marked by a step keyed by its call ID).
fn has_children(ctx: &RunContext, exchanges: &[Exchange]) -> bool {
    exchanges
        .iter()
        .flat_map(|exchange| &exchange.calls)
        .filter(|call| call.tool.0 == context::DELEGATE_TOOL)
        .any(|call| ctx.work.steps.iter().any(|step| step.key == call.call.0))
}

/// Emits total measured usage only when every turn on a childless first-claim run reported counts.
fn step_usage(
    previous: &[TurnUsage],
    turns: usize,
    children: bool,
    last: Option<TokenUsage>,
    first_claim: bool,
) -> Option<TokenUsage> {
    if !first_claim || children || previous.len() != turns {
        return None;
    }
    let mut total = last?;
    for usage in previous {
        let TurnUsage::Reported(turn) = usage else {
            return None;
        };
        total.prompt = total.prompt.checked_add(turn.prompt)?;
        total.completion = total.completion.checked_add(turn.completion)?;
    }
    Some(total)
}

fn model_error(control: &Control, error: ModelError) -> Flow {
    match error {
        ModelError::Cancelled if control.lost() => Flow::Lost,
        ModelError::Cancelled if control.cancelled() => Flow::Fail(Failure::Permanent("cancelled")),
        ModelError::Cancelled if control.shutdown() => Flow::Release,
        ModelError::Cancelled => Flow::Fail(Failure::Transient("model_cancelled")),
        ModelError::Parked => Flow::Parked,
        ModelError::Lost => Flow::Lost,
        other => Flow::Fail(other.failure()),
    }
}
