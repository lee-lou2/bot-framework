//! Worker tool-call and delegation dispatch through the broker and delegation store.

use super::{Flow, Worker, span::RunSpan};
use crate::model::{CallEnd, PendingCall};
use bot_core::{
    cli::CliRefusal,
    command::{CallResult, Delegate, Failure, JsonPayload, ToolCall},
    id::{Attempt, CallId, ToolId},
    port::{Error, ModelCall, Reply},
    profile::Snapshot,
    state::RunContext,
    trace::key,
};
use serde_json::Value;
use tokio::sync::watch;

/// Retry budget for `Error::Stale` caused by a concurrent revision change (a lost lease exits immediately).
const STALE_RETRIES: usize = 3;

pub(super) enum Called {
    Done(JsonPayload),
    Parked,
}

pub(super) enum CallError {
    Lost,
    Released,
    Denied,
    Missing,
    Invalid,
    Conflict,
    Transient(&'static str),
}

impl CallError {
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

pub(super) enum Ran {
    End(CallEnd),
    Refused(CliRefusal),
    Parked,
}

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
    pub(super) async fn call_tool(
        &self,
        released: Option<watch::Receiver<bool>>,
        attempt: &Attempt,
        call: &CallId,
        tool: &ToolId,
        args: &JsonPayload,
    ) -> Result<Called, CallError> {
        let result = self
            .call_with(released, attempt, || {
                self.deps.broker.call(ToolCall {
                    attempt: attempt.clone(),
                    call: call.clone(),
                    tool: tool.clone(),
                    args: args.clone(),
                })
            })
            .await?;
        Ok(match result {
            CallResult::Done(payload) => Called::Done(payload),
            CallResult::Awaiting(_) | CallResult::Held(_) | CallResult::Passed => Called::Parked,
        })
    }

    async fn call_with<'a, T>(
        &self,
        released: Option<watch::Receiver<bool>>,
        attempt: &Attempt,
        call: impl Fn() -> Reply<'a, T>,
    ) -> Result<T, CallError> {
        for _ in 0..STALE_RETRIES {
            let result = tokio::select! {
                biased;
                () = shutdown_requested(released.clone()) => return Err(CallError::Released),
                result = call() => result,
            };
            match result {
                Ok(value) => return Ok(value),
                Err(Error::Denied) => return Err(CallError::Denied),
                Err(Error::Missing) => return Err(CallError::Missing),
                Err(Error::Invalid) => return Err(CallError::Invalid),
                Err(Error::Conflict) => return Err(CallError::Conflict),
                Err(Error::Unavailable | Error::RateLimited) => {
                    return Err(CallError::Transient("broker_unavailable"));
                }
                // Distinguish a lost lease from a concurrent revision bump by renewing the lease.
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

    pub(super) async fn run_planned_call(
        &self,
        ctx: &RunContext,
        released: Option<watch::Receiver<bool>>,
        snapshot: &Snapshot,
        span: &RunSpan,
        call: &PendingCall,
    ) -> Result<Ran, Flow> {
        let key = CliRefusal::key(&call.call);
        if let Some(mark) = ctx.work.steps.iter().find(|s| s.key == key)
            && !ctx.work.steps.iter().any(|s| s.key == call.call.0)
        {
            // Replay stored refusal marks only when the ledger has no started/held/terminal effect.
            let known = self
                .deps
                .ledger
                .lookup(ctx.attempt.clone(), call.call.clone())
                .await
                .map_err(|e| match e {
                    Error::Stale => Flow::Lost,
                    _ => Flow::Fail(Failure::Transient("store_unavailable")),
                })?;
            if CliRefusal::can_replay(known.as_ref()) {
                let reason = CliRefusal::decode(&mark.body)
                    .map_err(|_| Flow::Fail(Failure::Permanent("corrupt_checkpoint")))?;
                return Ok(Ran::Refused(reason));
            }
        }
        self.run_call(ctx, released, snapshot, span, call).await
    }

    pub(super) async fn run_call(
        &self,
        ctx: &RunContext,
        released: Option<watch::Receiver<bool>>,
        snapshot: &Snapshot,
        span: &RunSpan,
        call: &PendingCall,
    ) -> Result<Ran, Flow> {
        let outcome = if call.tool.0 == crate::context::DELEGATE_TOOL {
            self.run_delegate(ctx, snapshot, span, call).await?
        } else {
            let req = ToolCall {
                attempt: ctx.attempt.clone(),
                call: call.call.clone(),
                tool: call.tool.clone(),
                args: call.args.clone(),
            };
            match self
                .call_with(released, &ctx.attempt, || {
                    self.deps.broker.call_for_model(req.clone())
                })
                .await
            {
                Ok(ModelCall::Returned(CallResult::Done(payload))) => {
                    Ran::End(CallEnd::Done(payload))
                }
                Ok(ModelCall::Returned(
                    CallResult::Awaiting(_) | CallResult::Held(_) | CallResult::Passed,
                )) => Ran::Parked,
                Ok(ModelCall::Refused(reason)) => Ran::Refused(reason),
                Err(error) => return end_or_stop(error),
            }
        };
        if let Ran::Refused(reason) = outcome {
            if ctx.work.steps.iter().any(|s| s.key == call.call.0) {
                return Ok(Ran::End(CallEnd::Denied));
            }
            self.checkpoint(ctx, reason.step(&call.call)).await?;
        }
        Ok(outcome)
    }

    async fn run_delegate(
        &self,
        ctx: &RunContext,
        snapshot: &Snapshot,
        span: &RunSpan,
        call: &PendingCall,
    ) -> Result<Ran, Flow> {
        let Some((child, instruction)) = Delegate::parse_target(&call.args) else {
            return Ok(Ran::Refused(CliRefusal::Invalid));
        };
        // Require direct-child authorization in both the pinned and active revisions, even when reading a finished child's result.
        let Some(current) = self.deps.catalog.current(&ctx.attempt.profile) else {
            return Ok(Ran::Refused(CliRefusal::Denied));
        };
        let Ok(scopes) = bot_core::scope::delegation_scopes(ctx, &current, snapshot, &child) else {
            return Ok(Ran::Refused(CliRefusal::Denied));
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
                Err(Error::Unavailable | Error::RateLimited) => {
                    Err(Flow::Fail(Failure::Transient("store_unavailable")))
                }
            };
        }
        let started = span
            .child("delegate", &call.call.0, "worker.delegate")
            .attr(key::BOT_DELEGATE_STEP, call.call.0.clone())
            .attr(key::BOT_DELEGATE_CHILD, child.0.clone());
        let result = self.deps.delegation.delegate(command).await;
        match result {
            Ok(_) => {
                span.emit(started);
                Ok(Ran::Parked)
            }
            Err(Error::Denied) => {
                span.emit(started.fail("denied"));
                Ok(Ran::Refused(CliRefusal::Denied))
            }
            Err(Error::Invalid | Error::Missing) => {
                span.emit(started.fail("invalid"));
                Err(Flow::Fail(Failure::Transient("store_unavailable")))
            }
            Err(Error::Conflict) => Err(Flow::Fail(Failure::Permanent("delegate_conflict"))),
            Err(Error::Unavailable | Error::RateLimited) => {
                Err(Flow::Fail(Failure::Transient("store_unavailable")))
            }
            Err(Error::Stale) => Err(Flow::Lost),
        }
    }
}

fn end_or_stop(error: CallError) -> Result<Ran, Flow> {
    match error {
        CallError::Denied => Ok(Ran::End(CallEnd::Denied)),
        CallError::Missing => Ok(Ran::End(CallEnd::Failed("unknown_tool".into()))),
        CallError::Invalid => Ok(Ran::End(CallEnd::Failed("invalid_args".into()))),
        other => Err(other.into_flow()),
    }
}

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
