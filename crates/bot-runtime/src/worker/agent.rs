//! 에이전트 대상 run 실행: 맥락 조립 → 모델 → (Step 방식) 도구 호출 중계·체크포인트 → 답.
//!
//! - **Step**: 워커가 차례마다 실행기를 부른다. 모델이 정한 차례(`turn-<n>`)를 호출 실행 **전에** 체크포인트로 적고, 호출이 모두 끝나면
//!   교환(`end-<n>`)을 적는다. 재개하면 끝난 차례는 되살리고, 멈춘 차례는 같은 호출 ID로 이어 간다(브로커 원장이 멱등이다).
//!   승인 대기·보류·위임으로 저장소가 run을 멈추면 이 시도는 거기서 끝난다.
//! - **Loop**(CLI): 실행기가 자기 도구 순환을 돌고 도구는 브로커 중계로 나간다. 워커는 한 번만 부르고 답을 받는다.

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
    model::{DoneCall, Exchange, ModelError, ModelRunner, ModelStep, RunnerKind, Session},
};
use bot_core::{
    command::{Failure, Finish, JsonPayload, RunOutcome, Step, TokenUsage},
    id::Generation,
    port::Error,
    profile::Snapshot,
    state::RunContext,
    trace::{key, now_nano},
};
use serde_json::json;
use std::{collections::HashSet, sync::Arc, time::SystemTime};

/// 대화로 보내는 답의 글자 수 상한
const MAX_ANSWER_CHARS: usize = 12_000;
/// 실행기가 차례마다 남기는 불투명 공급자 상태(체크포인트에 그대로 적는다)의 크기 상한
const MAX_STATE_BYTES: usize = 256 * 1024;

/// 최종 답을 끝맺는다. 호스트 상한(`MAX_ANSWER_CHARS`)을 넘어 실제로 잘렸으면 계량의 `truncated`로 같은 커밋에 남긴다(조용히 자르지 않는다).
/// `outcome`은 실행기가 아는 계량이다. `usage_whole`은 그 사용량이 이 run 전체의 합인가다(Loop는 첫 claim의 실행, Step은 `step_usage`가 합을 낸 경우): 아니면
/// (다시 집은 run·합을 알 수 없는 실행) 실행기가 보고한 사용량을 버린다.
fn answer(text: &str, mut outcome: RunOutcome, usage_whole: bool) -> Flow {
    let text = text.trim();
    if text.is_empty() {
        return Flow::Fail(Failure::Permanent("empty_answer"));
    }
    let clipped: String = text.chars().take(MAX_ANSWER_CHARS).collect();
    outcome.truncated |= clipped.len() < text.len();
    if !usage_whole {
        outcome.usage = None;
    }
    let payload = JsonPayload(json!({ "text": clipped }).to_string());
    Flow::Done(
        Finish {
            result: payload.clone(),
            answers: vec![payload],
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
        // 비신뢰 대화(인증된 API 출처의 루트)는 호출자가 보낸 대화가 전부다: 서버 이력을 읽지도 섞지도 않는다
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
        // 기억은 맥락을 보태는 것뿐이다: 저장소 사정으로 못 읽으면 기억 없이 진행한다(조회가 플래그를 먼저 커밋하므로 안전하다)
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
        // 이 시도의 프런트 연결(세션 토큰). CLI 실행이 끝나면(이 함수를 벗어나면) 이 시도가 발급받은 토큰만 폐기된다
        let grant = self
            .deps
            .relays
            .open(&ctx.attempt, &ctx.agent, &assembled.model);
        // 세션 복구 포트: 승인·위임으로 멈췄다 깨어난 run이 같은 대화를 잇게 한다(릴레이가 있을 때만 CLI가 돈다)
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
        let result = runner.step(request).await;
        self.model_span(span, 0, &alias, started, &result);
        match result {
            Ok(ModelStep::Final { text, outcome, .. }) => {
                answer(&text, outcome, ctx.attempt.generation == Generation(1))
            }
            // 자기 순환을 가진 실행기는 최종 답만 돌려준다
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
        // 차례마다 그 차례를 정한 응답의 사용량 기록(체크포인트에서 되살린 것 + 이번 시도에서 정한 것)
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
                    let result = runner.step(request).await;
                    self.model_span(span, turn, &assembled.model, started, &result);
                    match result {
                        Ok(ModelStep::Final { text, outcome, .. }) => {
                            // 이 응답의 사용량과 앞 차례들의 기록이 모두 있을 때만 run 전체의 합이다
                            let total = step_usage(
                                &usages,
                                exchanges.len(),
                                has_children(ctx, &exchanges),
                                outcome.usage,
                                ctx.attempt.generation == Generation(1),
                            );
                            return answer(
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
                            if let Err(flow) = self
                                .checkpoint(ctx, checkpoint::plan_key(turn), plan.to_payload())
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
                match self
                    .run_call(ctx, Some(control.released()), snapshot, span, call)
                    .await
                {
                    Ok(Ran::End(end)) => done.push(DoneCall {
                        call: call.call.clone(),
                        tool: call.tool.clone(),
                        args: call.args.clone(),
                        end,
                    }),
                    // 승인 대기·보류·위임: 저장소가 run을 멈췄다. 풀리면 같은 차례를 이어 한다
                    Ok(Ran::Parked) => return Flow::Parked,
                    Err(flow) => return flow,
                }
            }
            let exchange = Exchange {
                text: plan.text,
                calls: done,
                state: plan.state,
            };
            if let Err(flow) = self
                .checkpoint(ctx, checkpoint::end_key(turn), exchange.to_payload())
                .await
            {
                return flow;
            }
            exchanges.push(exchange);
        }
    }

    /// 체크포인트를 적는다. 리스를 잃었으면 물러나고, 저장소 사정이면 다시 시도하게 실패로 올린다.
    async fn checkpoint(
        &self,
        ctx: &RunContext,
        key: String,
        body: JsonPayload,
    ) -> Result<(), Flow> {
        match self
            .deps
            .runs
            .checkpoint(ctx.attempt.clone(), Step { key, body })
            .await
        {
            Ok(()) => Ok(()),
            Err(Error::Stale | Error::Denied | Error::Missing) => Err(Flow::Lost),
            Err(Error::Conflict) => Err(Flow::Fail(Failure::Permanent("checkpoint_conflict"))),
            Err(_) => Err(Flow::Fail(Failure::Transient("checkpoint_unavailable"))),
        }
    }

    fn model_span(
        &self,
        span: &RunSpan,
        turn: usize,
        alias: &str,
        started: u64,
        result: &Result<ModelStep, ModelError>,
    ) {
        let mut record = span
            .child("model", &turn.to_string(), "worker.model")
            .started_at(started)
            .attr(key::GEN_AI_OPERATION_NAME, "chat")
            .attr(key::GEN_AI_REQUEST_MODEL, alias.to_owned());
        match result {
            Ok(ModelStep::Final { usage, .. } | ModelStep::Calls { usage, .. }) => {
                // 상류가 보고하지 않은 사용량은 0으로 적지 않고 속성을 비운다(실제 요청은 입력도 출력도 0일 수 없다)
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

/// 이 run이 **실제로 자식을 만든** 위임을 했는가: 위임 호출의 호출 ID로 저장소가 적은 단계(`ctx.work.steps`)가 있다(`run_delegate`가 같은 표지로 «이미 위임했다»를 안다).
/// 자식을 만들지 못한 위임(대상 오류·거절)은 해당하지 않는다 — 그 호출의 모델 요청은 이 run의 합에 그대로 들어 있다.
fn has_children(ctx: &RunContext, exchanges: &[Exchange]) -> bool {
    exchanges
        .iter()
        .flat_map(|exchange| &exchange.calls)
        .filter(|call| call.tool.0 == context::DELEGATE_TOOL)
        .any(|call| ctx.work.steps.iter().any(|step| step.key == call.call.0))
}

/// Step 실행의 사용량 합계: 공급자가 보고한 값을 **모든 모델 요청**에서 모을 수 있을 때만이다. 앞 차례마다 체크포인트의 기록이 `Reported`이고 마지막 응답(`last`)도
/// 보고했고(`previous`는 앞 `turns`차례 전부의 기록이어야 한다), 이 run이 첫 claim이고(다시 집은 run은 응답을 받고도 기록하지 못한 요청이 있을 수 있다) 실제로 만든 자식이 없을 때
/// (`children`; 자식의 요청은 이 합에 없다)만 `Some`이다.
/// 하나라도 빠지면 `None`이다 — 마지막 차례만 합계로 내지 않는다.
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

/// 실행기 실패를 run의 흐름으로 옮긴다. 취소 신호로 멈춘 것은 리스를 잃은 것인지, 운영자 취소인지 가른다.
fn model_error(control: &Control, error: ModelError) -> Flow {
    match error {
        ModelError::Cancelled if control.lost() => Flow::Lost,
        ModelError::Cancelled if control.cancelled() => Flow::Fail(Failure::Permanent("cancelled")),
        // 종료 반납 신호로 멈췄다: 실행기가 프로세스를 끝내고 돌아왔다. 정리를 확인한 뒤 대기열로 돌려준다
        ModelError::Cancelled if control.shutdown() => Flow::Release,
        ModelError::Cancelled => Flow::Fail(Failure::Transient("model_cancelled")),
        // 세션을 잇다 승인·자식을 다시 기다려야 한다: 저장소가 run을 멈췄다
        ModelError::Parked => Flow::Parked,
        ModelError::Lost => Flow::Lost,
        other => Flow::Fail(other.failure()),
    }
}
