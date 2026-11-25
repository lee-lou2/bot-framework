//! 도구 호출과 위임 호출의 중계. 권한 판정은 브로커가 호출마다 다시 한다 — 여기서는 결과를 모델·run이 쓸 모양으로 옮길 뿐이다.

use super::{Flow, Worker, span::RunSpan};
use crate::model::{CallEnd, PendingCall};
use bot_core::{
    command::{CallResult, Delegate, Failure, JsonPayload, ToolCall},
    id::{Attempt, CallId, ToolId},
    port::Error,
    profile::Snapshot,
    state::RunContext,
    trace::key,
};
use serde_json::Value;
use tokio::sync::watch;

/// 판정에 쓴 판본이 바뀐 `Stale`을 다시 시도하는 횟수(리스를 잃은 `Stale`은 바로 물러난다).
const STALE_RETRIES: usize = 3;

pub(super) enum Called {
    Done(JsonPayload),
    /// 승인 대기·보류: 저장소가 run을 멈췄다(리스를 놓았다).
    Parked,
}

/// 호출 실패. 모델이 볼 결말(`Denied`·`Missing`·`Invalid`)과 run 전체의 사정(`Lost`·`Conflict`·`Transient`)을 가른다.
pub(super) enum CallError {
    Lost,
    /// 종료 반납 신호로 호출을 끊었다(정리가 프로세스 종료를 확인한 뒤 대기열로 돌려준다).
    Released,
    Denied,
    Missing,
    Invalid,
    Conflict,
    Transient(&'static str),
}

impl CallError {
    /// 도구를 직접 부르는 run에서는 호출 실패가 곧 run의 실패다.
    pub(super) fn into_flow(self) -> Flow {
        match self {
            CallError::Lost => Flow::Lost,
            CallError::Released => Flow::Release,
            CallError::Denied => Flow::Fail(Failure::Permanent("denied")),
            CallError::Missing => Flow::Fail(Failure::Permanent("unknown_tool")),
            CallError::Invalid => Flow::Fail(Failure::Permanent("invalid_args")),
            CallError::Conflict => Flow::Fail(Failure::Permanent("call_conflict")),
            CallError::Transient(code) => Flow::Fail(Failure::Transient(code)),
        }
    }
}

/// 모델 차례 안의 호출 하나의 결과.
pub(super) enum Ran {
    End(CallEnd),
    Parked,
}

/// 종료 반납 신호가 올 때까지 기다린다(신호가 없거나 오지 않으면 영원히).
async fn shutdown_requested(signal: Option<watch::Receiver<bool>>) {
    let Some(mut signal) = signal else {
        return std::future::pending().await;
    };
    loop {
        if *signal.borrow_and_update() {
            return;
        }
        if signal.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl Worker {
    /// `released`가 있으면 종료 반납 신호가 올 때 기다리던 호출을 놓는다(없으면 끝까지 기다린다 — CLI 세션 복원처럼 실행기가 직접 끊는 경로).
    pub(super) async fn call_tool(
        &self,
        released: Option<watch::Receiver<bool>>,
        attempt: &Attempt,
        call: &CallId,
        tool: &ToolId,
        args: &JsonPayload,
    ) -> Result<Called, CallError> {
        for _ in 0..STALE_RETRIES {
            // 종료 반납 신호가 오면 기다리던 호출을 놓는다: 시작한 도구가 남았으면 정리가 `reap`으로 끝내고, 다음 점유의 브로커가 그 호출을 복구한다
            let result = tokio::select! {
                biased;
                () = shutdown_requested(released.clone()) => return Err(CallError::Released),
                result = self.deps.broker.call(ToolCall {
                    attempt: attempt.clone(),
                    call: call.clone(),
                    tool: tool.clone(),
                    args: args.clone(),
                }) => result,
            };
            match result {
                Ok(CallResult::Done(payload)) => return Ok(Called::Done(payload)),
                Ok(CallResult::Awaiting(_) | CallResult::Held(_)) => return Ok(Called::Parked),
                // 넘김: 저장소가 호출 결론·커서 전진과 함께 run을 다음 후보(또는 기본 대화)로 되돌렸다 — 이 시도는 더 쓸 것이 없다
                Ok(CallResult::Passed) => return Ok(Called::Parked),
                Err(Error::Denied) => return Err(CallError::Denied),
                Err(Error::Missing) => return Err(CallError::Missing),
                Err(Error::Invalid) => return Err(CallError::Invalid),
                Err(Error::Conflict) => return Err(CallError::Conflict),
                Err(Error::Unavailable) => return Err(CallError::Transient("broker_unavailable")),
                // 리스를 잃은 것인지(물러난다), 판정에 쓴 판본이 그 사이 바뀐 것인지(다시 부른다) 리스로 가른다
                Err(Error::Stale) => {
                    if self.lost_lease(attempt).await {
                        return Err(CallError::Lost);
                    }
                }
            }
        }
        Err(CallError::Transient("stale_revision"))
    }

    async fn lost_lease(&self, attempt: &Attempt) -> bool {
        matches!(
            self.deps.runs.renew(attempt.clone()).await,
            Err(Error::Stale | Error::Denied | Error::Missing)
        )
    }

    /// 모델이 요청한 호출 하나를 실행한다(위임 도구는 가로채 위임으로 보낸다).
    pub(super) async fn run_call(
        &self,
        ctx: &RunContext,
        released: Option<watch::Receiver<bool>>,
        snapshot: &Snapshot,
        span: &RunSpan,
        call: &PendingCall,
    ) -> Result<Ran, Flow> {
        if call.tool.0 == crate::context::DELEGATE_TOOL {
            return self.run_delegate(ctx, snapshot, span, call).await;
        }
        match self
            .call_tool(released, &ctx.attempt, &call.call, &call.tool, &call.args)
            .await
        {
            Ok(Called::Done(payload)) => Ok(Ran::End(CallEnd::Done(payload))),
            Ok(Called::Parked) => Ok(Ran::Parked),
            Err(error) => end_or_stop(error),
        }
    }

    async fn run_delegate(
        &self,
        ctx: &RunContext,
        snapshot: &Snapshot,
        span: &RunSpan,
        call: &PendingCall,
    ) -> Result<Ran, Flow> {
        let Some((child, instruction)) = Delegate::parse_target(&call.args) else {
            return Ok(Ran::End(CallEnd::Failed("invalid_args".into())));
        };
        // 두 판본(실행이 고정한 판본·지금 활성인 판본) 모두에서 직계 자식이어야 한다. 범위는 둘 다에서 허용된 만큼으로 좁힌다.
        // 이미 끝난 자식의 결과를 돌려주는 때도 같다: 위임한 뒤 권한이 바뀌었으면 그 결과를 모델에게 주지 않는다
        let Some(current) = self.deps.catalog.current(&ctx.attempt.profile) else {
            return Ok(Ran::End(CallEnd::Denied));
        };
        let Ok(scopes) = bot_core::scope::delegation_scopes(ctx, &current, snapshot, &child) else {
            return Ok(Ran::End(CallEnd::Denied));
        };
        let command = Delegate {
            parent: ctx.attempt.clone(),
            checked_revision: current.revision.clone(),
            step: call.call.0.clone(),
            child: child.clone(),
            instruction,
            scopes,
        };
        if ctx.work.steps.iter().any(|s| s.key == call.call.0) {
            return match self.deps.delegation.result(command).await {
                Ok(Some(body)) => Ok(Ran::End(child_end(&body))),
                Ok(None) | Err(Error::Missing | Error::Conflict | Error::Invalid) => {
                    Ok(Ran::End(CallEnd::Failed("invalid_child_result".into())))
                }
                Err(Error::Denied) => Ok(Ran::End(CallEnd::Denied)),
                Err(Error::Stale) => Err(Flow::Lost),
                Err(Error::Unavailable) => Err(Flow::Fail(Failure::Transient("store_unavailable"))),
            };
        }
        let started = span
            .child("delegate", &call.call.0, "worker.delegate")
            .attr(key::BOT_DELEGATE_STEP, call.call.0.clone())
            .attr(key::BOT_DELEGATE_CHILD, child.0.clone());
        let result = self.deps.delegation.delegate(command).await;
        match result {
            // 자식 생성과 부모 대기가 한 커밋이다: 이 시도는 여기서 끝난다
            Ok(_) => {
                span.emit(started);
                Ok(Ran::Parked)
            }
            Err(Error::Denied) => {
                span.emit(started.fail("denied"));
                Ok(Ran::End(CallEnd::Denied))
            }
            Err(Error::Invalid | Error::Missing) => {
                span.emit(started.fail("invalid"));
                Ok(Ran::End(CallEnd::Failed("invalid_args".into())))
            }
            Err(Error::Conflict) => Err(Flow::Fail(Failure::Permanent("delegate_conflict"))),
            Err(Error::Unavailable) => Err(Flow::Fail(Failure::Transient("store_unavailable"))),
            Err(Error::Stale) => Err(Flow::Lost),
        }
    }
}

/// 모델에게 돌려줄 결말로 옮길 수 있는 호출 실패는 결말로, 아니면 run을 멈춘다.
fn end_or_stop(error: CallError) -> Result<Ran, Flow> {
    match error {
        CallError::Denied => Ok(Ran::End(CallEnd::Denied)),
        CallError::Missing => Ok(Ran::End(CallEnd::Failed("unknown_tool".into()))),
        CallError::Invalid => Ok(Ran::End(CallEnd::Failed("invalid_args".into()))),
        other => Err(other.into_flow()),
    }
}

/// 자식 결과 체크포인트(`{"state":"succeeded","result":…}` · `{"state":"dead"|"cancelled","error":코드}`)를 모델이 볼 결말로 옮긴다.
fn child_end(body: &JsonPayload) -> CallEnd {
    let Ok(value) = serde_json::from_str::<Value>(&body.0) else {
        return CallEnd::Failed("child_failed".into());
    };
    match value.get("state").and_then(Value::as_str) {
        Some("succeeded") => CallEnd::Done(JsonPayload(
            value.get("result").unwrap_or(&Value::Null).to_string(),
        )),
        Some("cancelled") => CallEnd::Failed("child_cancelled".into()),
        _ => CallEnd::Failed("child_failed".into()),
    }
}
