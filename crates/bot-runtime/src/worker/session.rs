//! Loop 방식(CLI) 실행기에 주는 세션 포트. 실행기가 세션을 되살리거나 따라 쓸 때 필요한 호스트 일을 맡는다:
//! 체크포인트 묶음 쓰기, 프런트가 확인한 응답 읽기, 호출 결과의 진위 확인, 아직 결과 없는 자리를 현재 권한·승인으로 마무리하기.
//! 호출은 `CallId::cli(turn, pos)`로 라이브 경로(브로커 프런트)와 같은 호출이라 원장이 중복 실행을 막는다.

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
    /// `mcp__<서버>__`: CLI가 본 릴레이 도구 이름의 접두사. 그 밖의 이름은 VM 내장 도구다.
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
    /// 프런트가 이 자리에 남긴 확정 거절(고정 판본에 없는 도구·입력 검사 실패)의 표식. 체크포인트의 JSON 문자열이고, 읽을 수 없는 표식은 없는 것으로 본다.
    async fn refusal_mark(&self, call: &CallId) -> Result<Option<CliRefusal>, SessionError> {
        let stored = self
            .worker
            .deps
            .ledger
            .step(self.ctx.attempt.clone(), CliRefusal::key(call))
            .await
            .map_err(session_error)?;
        Ok(stored.and_then(|payload| serde_json::from_str::<CliRefusal>(&payload.0).ok()))
    }

    /// 원장에 기록이 없거나 아직 열려 있는 호출에 남은 확정 거절. 원장에 시작·보류·완료·거절이 있으면 표식이 아니라 원장을 따른다(모순이면 원장이 더 구체적이다).
    async fn refused_open_call(
        &self,
        call: &CallId,
        record: &Option<CallRecord>,
    ) -> Result<Option<CliRefusal>, SessionError> {
        match record {
            None
            | Some(CallRecord {
                phase: CallPhase::Open { .. },
                ..
            }) => self.refusal_mark(call).await,
            _ => Ok(None),
        }
    }

    /// 위임 슬롯의 결말(새 위임·부모 대기 없음). 순서는 ① 기존 자식: 저장소가 대상·지시·부여 범위를 대조한 완료 결과(어긋나면 `denied`)가 먼저다.
    /// ② 자식이 없고 프런트가 이 자리를 확정 거절로 닫았으면(표식) 권한·판본이 지금 어떻든 모델이 처음 본 그 단어다: 권한을 다시 얻어도 같은 단어이고 자식은 만들지 않는다.
    /// ③ 둘 다 없으면 `Nothing`이다. 표식이 없을 때의 동작(현재·고정 판본 모두의 직계 인가, 입력 검사)은 그대로다.
    async fn recall_child(&self, slot: &Pending, call: &CallId) -> Result<Delegated, SessionError> {
        let mark = self.refusal_mark(call).await?;
        // 표식이 있으면 합성 거절(입력·인가)도 그 단어로 한다: 모델이 본 것과 같아야 한다
        let synthetic = |fallback: &'static str| match mark {
            Some(refusal) => Delegated::Refused(refusal),
            None => Delegated::Result(ToolOutput::notice(fallback)),
        };
        let Some((child, instruction)) = Delegate::target(&slot.tool_use.input) else {
            return Ok(synthetic("invalid"));
        };
        let Some(current) = self.worker.deps.catalog.current(&self.ctx.attempt.profile) else {
            return Ok(synthetic("denied"));
        };
        let Ok(scopes) =
            bot_core::scope::delegation_scopes(&self.ctx, &current, &self.snapshot, &child)
        else {
            return Ok(synthetic("denied"));
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
            Ok(None) => Ok(mark.map_or(Delegated::Nothing, Delegated::Refused)),
            // 기존 자식과 어긋난다: 저장소가 더 구체적이라 표식이 있어도 `denied`다
            Err(Error::Denied) => Ok(Delegated::Result(ToolOutput::notice("denied"))),
            Err(error) => Err(session_error(error)),
        }
    }
}

/// 위임 슬롯의 결말(`recall_child`).
enum Delegated {
    /// 기존 자식의 완료 결과, 또는 표식이 없는 합성 거절(`denied`·`invalid`)
    Result(ToolOutput),
    /// 자식이 없고 프런트가 확정 거절로 닫았다
    Refused(CliRefusal),
    /// 아직 끝난 것이 없다
    Nothing,
}

/// 저장소·원장 오류를 세션 오류로: 소유권을 잃었으면 물러나고, 사정이면 다시 하고, 입력이 틀렸으면 영구다.
fn session_error(error: Error) -> SessionError {
    match error {
        Error::Stale | Error::Denied | Error::Missing => SessionError::Lost,
        Error::Conflict => SessionError::Permanent("checkpoint_conflict"),
        Error::Invalid => SessionError::Permanent("invalid_checkpoint"),
        Error::Unavailable => SessionError::Unavailable,
    }
}

/// 자식 결과 체크포인트(`{"state":"succeeded","result":…}` · `{"state":"dead"|"cancelled","error":코드}`)를 모델이 볼 결과로(③ 프런트와 같은 규칙).
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

/// 프런트가 계획·원장에 닿기 전에 고정 단어로 거절하는 호출: 풀 수 없는 이름·객체가 아닌 인자·호스트 전용 도구.
/// 모델이 처음 본 것과 같은 단어로 되살린다. 실행할 것이 없으니 원장에 기록이 없어도 끝난 호출이다.
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

    /// 이미 기록된 자리를 원래 계획으로 새 실행 없이 다시 읽는다: 원장에서 끝난 호출만 현재 권한·도구·인자로 다시 확인해 값을 돌려주고
    /// (거절은 고정 단어), 위임은 저장소가 자식의 대상·지시·부여 범위를 대조한 완료 결과만 읽는다. 끝나지 않았으면 `None`이다.
    fn recall<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Option<ToolOutput>> {
        Box::pin(async move {
            let Some(mcp) = slot.tool_use.name.strip_prefix(&self.prefix) else {
                return Ok(None); // VM 내장 도구는 호스트가 아는 결과가 없다
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
            // 원장에 끝난 기록이 있을 때만 읽는다(새로 실행하지 않는다). 끝난 호출은 브로커로 다시 확인해 같은 값을 돌려받고,
            // 확정 거절(승인 거절·권한 거절)은 같은 도구·인자일 때 실행 없이 거절 단어로 복원한다(재개를 몇 번 해도 같은 결과다)
            match record {
                Some(CallRecord {
                    phase: CallPhase::Done(_),
                    ..
                }) => {}
                // 거절 기록의 지문은 승인 지문이다(기억 공유는 본문 지문이, 예약 생성은 확정된 시간대가 더해져 모델 입력 해시와 다르다).
                // 그래서 지문이 아니라 도구로만 이 자리의 기록임을 확인한다: 자리는 호스트가 저장한 계획(이름·입력)이 정하고, 기록된 호출 ID와 도구가 같으며,
                // 거절은 고정 단어일 뿐이라 다시 읽을 값이 없다. 끝난 호출의 재읽기는 브로커가 같은 호출의 요청 일치를 다시 확인한다.
                Some(CallRecord {
                    phase: CallPhase::Denied,
                    tool: recorded,
                    ..
                }) if recorded == tool => {
                    return Ok(Some(ToolOutput::notice("denied")));
                }
                // 실행되지 않은 호출: 프런트가 확정 거절을 남겼으면 모델이 처음 본 그 단어로 닫는다(실행 없음)
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
                // `Released`는 종료 반납 신호가 없는 이 경로(`None`)에서는 오지 않는다
                Err(CallError::Lost | CallError::Released) => Err(SessionError::Lost),
                Err(CallError::Transient(_)) => Err(SessionError::Unavailable),
            }
        })
    }

    /// 끝난 호출인가(재읽기·실행 없이 기록만 본다): 원장의 완료·확정 거절, 자식의 완료 결과, 그리고 실행할 것이 없는 자리(VM 내장·모르는 도구·잘못된 입력)다.
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
            // 실행되지 않았어도 프런트가 확정 거절로 닫았으면 끝난 호출이다
            Ok(self.refused_open_call(&call, &record).await?.is_some())
        })
    }

    fn settle<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Slot> {
        Box::pin(async move {
            let name = &slot.tool_use.name;
            let Some(mcp) = name.strip_prefix(&self.prefix) else {
                // VM 안에서 도는 내장 도구: 호스트에서 실행하지 않는다. 믿을 결과가 기록에 없으면 중단으로 닫는다
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
            // 프런트가 확정 거절을 남긴 자리는 실행하지 않고 그 단어로 닫는다(원장에 시작·보류가 없을 때만).
            // 위임은 기존 자식이 없을 때만: 권한을 다시 얻었어도 같은 단어를 되살리고 자식을 만들지 않는다
            if mcp == DELEGATE_TOOL {
                // 저장소 대조는 한 번이다: 끝난 자식·저장소의 거절·합성 거절·프런트의 확정 거절을 모두 이 판정으로 닫는다(소유권을 잃었으면 물러난다).
                // 끝난 것이 없을 때만 아래에서 새 위임을 만들거나(없던 자리) 부모 대기를 잇는다
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
                // 승인 대기·보류·위임: 저장소가 run을 멈췄다
                Ok(Ran::Parked) => Ok(Slot::Parked),
                Err(Flow::Lost) => Err(SessionError::Lost),
                Err(Flow::Fail(Failure::Permanent(code))) => Err(SessionError::Permanent(code)),
                Err(_) => Err(SessionError::Unavailable),
            }
        })
    }
}
