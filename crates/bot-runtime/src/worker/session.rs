//! Host `SessionPort` implementation bridging loop-mode CLI session checkpointing, recall, and slot settlement.

use super::{
    Flow, Worker,
    calls::{CallError, Called, Ran},
    span::RunSpan,
};
use crate::{
    context::DELEGATE_TOOL,
    model::{
        CallEnd, PendingCall, PortFuture, SessionError, SessionPort, Slot, ToolOutput,
        session::Pending,
    },
};
use bot_core::{
    canon::canonicalize,
    cli::{CliRefusal, CliResponse, RESPONSE_PREFIX},
    command::{Delegate, Failure, JsonPayload, Step},
    id::{CallId, ToolId},
    ledger::{CallPhase, CallRecord},
    port::Error,
    profile::Snapshot,
    state::RunContext,
};
use std::sync::Arc;

pub(super) struct RunSession {
    worker: Worker,
    ctx: RunContext,
    snapshot: Arc<Snapshot>,
    span: RunSpan,
    /// `mcp__<server>__` prefix identifying relay tools; all other tool names are VM built-ins.
    prefix: String,
}

impl RunSession {
    pub(super) fn new(
        worker: Worker,
        ctx: RunContext,
        snapshot: Arc<Snapshot>,
        relay_name: &str,
    ) -> Self {
        let span = RunSpan::begin(worker.deps.tracer.clone(), &ctx);
        Self {
            worker,
            ctx,
            snapshot,
            span,
            prefix: format!("mcp__{relay_name}__"),
        }
    }
}

impl RunSession {
    async fn refusal_mark(&self, call: &CallId) -> Result<Option<CliRefusal>, SessionError> {
        let stored = self
            .worker
            .deps
            .ledger
            .step(self.ctx.attempt.clone(), CliRefusal::key(call))
            .await
            .map_err(session_error)?;
        stored
            .as_ref()
            .map(CliRefusal::decode)
            .transpose()
            .map_err(|_| SessionError::Permanent("corrupt_checkpoint"))
    }

    async fn refused_open_call(
        &self,
        call: &CallId,
        record: &Option<CallRecord>,
    ) -> Result<Option<CliRefusal>, SessionError> {
        if CliRefusal::can_replay(record.as_ref()) {
            self.refusal_mark(call).await
        } else {
            Ok(None)
        }
    }

    /// Resolves a delegate slot without creating a new child: existing child results/mismatches take precedence over a stored refusal mark.
    async fn recall_child(&self, slot: &Pending, call: &CallId) -> Result<Delegated, SessionError> {
        let Some((child, instruction)) = Delegate::target(&slot.tool_use.input) else {
            return self.child_refusal(call, Some("invalid")).await;
        };
        let Some(current) = self.worker.deps.catalog.current(&self.ctx.attempt.profile) else {
            return self.child_refusal(call, Some("denied")).await;
        };
        let Ok(scopes) =
            bot_core::scope::delegation_scopes(&self.ctx, &current, &self.snapshot, &child)
        else {
            return self.child_refusal(call, Some("denied")).await;
        };
        let command = Delegate {
            parent: self.ctx.attempt.clone(),
            checked_revision: current.revision.clone(),
            step: call.0.clone(),
            child,
            instruction,
            scopes,
        };
        match self.worker.deps.delegation.result(command).await {
            Ok(Some(body)) => Ok(Delegated::Result(child_output(&body))),
            Ok(None) => self.child_refusal(call, None).await,
            Err(Error::Denied) => Ok(Delegated::Result(ToolOutput::notice("denied"))),
            Err(error) => Err(session_error(error)),
        }
    }

    async fn child_refusal(
        &self,
        call: &CallId,
        fallback: Option<&'static str>,
    ) -> Result<Delegated, SessionError> {
        Ok(match (self.refusal_mark(call).await?, fallback) {
            (Some(refusal), _) => Delegated::Refused(refusal),
            (None, Some(word)) => Delegated::Result(ToolOutput::notice(word)),
            (None, None) => Delegated::Nothing,
        })
    }
}

enum Delegated {
    Result(ToolOutput),
    Refused(CliRefusal),
    Nothing,
}

fn session_error(error: Error) -> SessionError {
    match error {
        Error::Stale | Error::Denied | Error::Missing => SessionError::Lost,
        Error::Conflict => SessionError::Permanent("checkpoint_conflict"),
        Error::Invalid => SessionError::Permanent("invalid_checkpoint"),
        Error::Unavailable | Error::RateLimited => SessionError::Unavailable,
    }
}

fn child_output(body: &JsonPayload) -> ToolOutput {
    let value: serde_json::Value = serde_json::from_str(&body.0).unwrap_or_default();
    match value.get("state").and_then(serde_json::Value::as_str) {
        Some("succeeded") => ToolOutput::child_result(&value["result"]),
        Some("cancelled") => ToolOutput::notice("child_cancelled"),
        _ => ToolOutput::notice("child_failed"),
    }
}

fn word(word: &'static str) -> Slot {
    Slot::Result(ToolOutput::notice(word))
}

/// Calls rejected by the relay front before reaching the ledger (unresolvable tool name, non-object args, host-only tool).
fn front_refusal(mcp: &str, input: &serde_json::Value) -> Option<&'static str> {
    let Some(tool) = ToolId::from_mcp_name(mcp) else {
        return Some("invalid");
    };
    if !input.is_object() || canonicalize(input).is_none() {
        return Some("invalid");
    }
    (tool.0 == bot_core::schedule::DELIVER_TOOL).then_some("unknown_tool")
}

impl SessionPort for RunSession {
    fn steps(&self) -> &[Step] {
        &self.ctx.work.steps
    }

    fn commit(&self, steps: Vec<Step>) -> PortFuture<'_, ()> {
        Box::pin(async move {
            self.worker
                .deps
                .runs
                .checkpoint_batch(self.ctx.attempt.clone(), steps)
                .await
                .map_err(session_error)
        })
    }

    fn response(&self, message_id: &str) -> PortFuture<'_, Option<CliResponse>> {
        let key = format!("{RESPONSE_PREFIX}{message_id}");
        Box::pin(async move {
            let stored = self
                .worker
                .deps
                .ledger
                .step(self.ctx.attempt.clone(), key)
                .await
                .map_err(session_error)?;
            let Some(payload) = stored else {
                return Ok(None);
            };
            match serde_json::from_str::<CliResponse>(&payload.0) {
                Ok(response) if response.is_valid() => Ok(Some(response)),
                _ => Err(SessionError::Permanent("cli_response_invalid")),
            }
        })
    }

    /// Reads an already-finished call from host records without executing any new work.
    fn recall<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Option<ToolOutput>> {
        Box::pin(async move {
            let Some(mcp) = slot.tool_use.name.strip_prefix(&self.prefix) else {
                return Ok(None);
            };
            let call = CallId::cli(slot.turn, slot.pos);
            if mcp == DELEGATE_TOOL {
                return Ok(match self.recall_child(slot, &call).await? {
                    Delegated::Result(output) => Some(output),
                    Delegated::Refused(refusal) => Some(ToolOutput::notice(refusal.word())),
                    Delegated::Nothing => None,
                });
            }
            if let Some(refusal) = front_refusal(mcp, &slot.tool_use.input) {
                return Ok(Some(ToolOutput::notice(refusal)));
            }
            let (Some(tool), Some(args)) = (
                ToolId::from_mcp_name(mcp),
                canonicalize(&slot.tool_use.input),
            ) else {
                return Ok(None);
            };
            let record = self
                .worker
                .deps
                .ledger
                .lookup(self.ctx.attempt.clone(), call.clone())
                .await
                .map_err(session_error)?;
            match record {
                Some(CallRecord {
                    phase: CallPhase::Done(_),
                    ..
                }) => {}
                // Match denied records by tool ID rather than args digest because approval digests include extra sealed context (e.g. memory body or resolved timezone).
                Some(CallRecord {
                    phase: CallPhase::Denied,
                    tool: recorded,
                    ..
                }) if recorded == tool => {
                    return Ok(Some(ToolOutput::notice("denied")));
                }
                _ => {
                    return Ok(self
                        .refused_open_call(&call, &record)
                        .await?
                        .map(|refusal| ToolOutput::notice(refusal.word())));
                }
            }
            match self
                .worker
                .call_tool(None, &self.ctx.attempt, &call, &tool, &JsonPayload(args))
                .await
            {
                Ok(Called::Done(payload)) => Ok(Some(ToolOutput::from_payload(&payload.0))),
                Ok(Called::Parked) => Ok(None),
                Err(CallError::Denied) => Ok(Some(ToolOutput::notice("denied"))),
                Err(CallError::Missing) => Ok(Some(ToolOutput::notice("unknown_tool"))),
                Err(CallError::Invalid) => Ok(Some(ToolOutput::notice("invalid"))),
                Err(CallError::Conflict) => Ok(Some(ToolOutput::notice("conflict"))),
                Err(CallError::Lost | CallError::Released) => Err(SessionError::Lost),
                Err(CallError::Transient(_)) => Err(SessionError::Unavailable),
            }
        })
    }

    fn ended<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, bool> {
        Box::pin(async move {
            let Some(mcp) = slot.tool_use.name.strip_prefix(&self.prefix) else {
                return Ok(true);
            };
            let call = CallId::cli(slot.turn, slot.pos);
            if mcp == DELEGATE_TOOL {
                return Ok(!matches!(
                    self.recall_child(slot, &call).await?,
                    Delegated::Nothing
                ));
            }
            if front_refusal(mcp, &slot.tool_use.input).is_some() {
                return Ok(true);
            }
            let record = self
                .worker
                .deps
                .ledger
                .lookup(self.ctx.attempt.clone(), call.clone())
                .await
                .map_err(session_error)?;
            if matches!(
                record,
                Some(CallRecord {
                    phase: CallPhase::Done(_) | CallPhase::Denied,
                    ..
                })
            ) {
                return Ok(true);
            }
            Ok(self.refused_open_call(&call, &record).await?.is_some())
        })
    }

    fn settle<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Slot> {
        Box::pin(async move {
            let name = &slot.tool_use.name;
            let Some(mcp) = name.strip_prefix(&self.prefix) else {
                // Host never runs VM built-ins; close unanswered built-in slots as aborted.
                return Ok(word("aborted"));
            };
            if mcp != DELEGATE_TOOL
                && let Some(refusal) = front_refusal(mcp, &slot.tool_use.input)
            {
                return Ok(word(refusal));
            }
            let tool = if mcp == DELEGATE_TOOL {
                ToolId(DELEGATE_TOOL.into())
            } else if let Some(tool) = ToolId::from_mcp_name(mcp) {
                tool
            } else {
                return Ok(word("invalid"));
            };
            let Some(args) = canonicalize(&slot.tool_use.input) else {
                return Ok(word("invalid"));
            };
            if mcp == DELEGATE_TOOL {
                let call = CallId::cli(slot.turn, slot.pos);
                match self.recall_child(slot, &call).await? {
                    Delegated::Result(output) => return Ok(Slot::Result(output)),
                    Delegated::Refused(refusal) => return Ok(word(refusal.word())),
                    Delegated::Nothing => {}
                }
            } else {
                let call = CallId::cli(slot.turn, slot.pos);
                let record = self
                    .worker
                    .deps
                    .ledger
                    .lookup(self.ctx.attempt.clone(), call.clone())
                    .await
                    .map_err(session_error)?;
                if let Some(refusal) = self.refused_open_call(&call, &record).await? {
                    return Ok(word(refusal.word()));
                }
            }
            let call = PendingCall {
                call: CallId::cli(slot.turn, slot.pos),
                tool,
                args: JsonPayload(args),
            };
            match self
                .worker
                .run_call(&self.ctx, None, &self.snapshot, &self.span, &call)
                .await
            {
                Ok(Ran::End(CallEnd::Done(payload))) if mcp == DELEGATE_TOOL => {
                    let value = serde_json::from_str(&payload.0)
                        .map_err(|_| SessionError::Permanent("invalid_child_result"))?;
                    Ok(Slot::Result(ToolOutput::child_result(&value)))
                }
                Ok(Ran::End(CallEnd::Done(payload))) => {
                    Ok(Slot::Result(ToolOutput::from_payload(&payload.0)))
                }
                Ok(Ran::Refused(reason)) => Ok(word(reason.word())),
                Ok(Ran::End(CallEnd::Denied)) => Ok(word("denied")),
                Ok(Ran::End(CallEnd::Failed(code))) => Ok(word(match code.as_str() {
                    "invalid_args" => "invalid",
                    "unknown_tool" => "unknown_tool",
                    "child_cancelled" => "child_cancelled",
                    _ => "child_failed",
                })),
                Ok(Ran::Parked) => Ok(Slot::Parked),
                Err(Flow::Lost) => Err(SessionError::Lost),
                Err(Flow::Fail(Failure::Permanent(code))) => Err(SessionError::Permanent(code)),
                Err(_) => Err(SessionError::Unavailable),
            }
        })
    }
}
