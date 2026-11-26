//! 내장 `schedule.create`·`schedule.list`·`schedule.cancel`: 대화에서 만드는 예약.
//!
//! - 각 도구는 같은 이름의 스코프가 필요하고 일반 호출과 같은 경계(요청자 ∩ 에이전트 상한 ∩ 정책)를 지난다. 예약 주체는 인증된 사람이다
//!   (서비스 주체·HTTP 주체는 만들 수 없다). CLI·HTTP 입구는 원래 대화에만 답한다. Slack의 명시 목적지는 같은 프로필 등록부에서
//!   실제 팀·채널을 확인하고 생성·발송 때 각각 소유자 승인을 받는다. 경로를 확인할 수 없으면 등록을 거절한다.
//! - 셋 모두 논리 호출 ID로 호출 원장에 기록한다(런타임이 다음 도구 계획을 저장하고 재개할 때 이 결과를 쓴다). 생성·취소는
//!   `UserSchedules`가 승인 소비·호출 완료·감사를 한 커밋으로 쓰고(일반 `start`로는 시작하지 못한다), 목록은 읽기라 일반 `start`·`finish`로
//!   기록한다: 현재 claim·활성 판본을 저장소가 확인하고 본인 활성 예약의 ID·시각·quiet만 돌려준다. 같은 호출의 재전달은 기록한 결과를
//!   현재 권한을 다시 확인한 뒤 그대로 쓴다.
//! - 인자는 엄격히 읽는다(`args`). 시각·cron 같은 시간에 기대는 검사는 이미 완료한 호출의 재전달을 막지 않도록 영수증을 먼저 찾은 뒤에 한다.
//!   생략한 시간대는 실행이 고정한 판본의 프로필 시간대로 봉인 전에 채워 카드·해시·저장이 같은 값을 쓴다.
//! - `schedule.deliver`는 호스트 전용 ID라 모델이 부를 수 없다(`Missing`).

use super::{
    BrokerService,
    call::{Flow, Note},
    gate::Gate,
    route::Route,
};
use crate::{approver::approver_for, classify::classify, resolve::Resolved, scope::authorize};
use bot_core::{
    command::{CallResult, JsonPayload, ToolCall},
    id::*,
    ledger::{CallPhase, Outcome, StartCall},
    port::Error,
    profile::{Scopes, Snapshot, Tool},
    schedule::{DeliveryTarget, ScheduleSpec},
    state::{ConversationKind, Risk, RunContext},
};

mod args;
use args::{Create, Reject, legacy_create_sha256, parse_cancel, parse_list, render_list};

pub use bot_core::builtin::{CANCEL_TOOL, CREATE_TOOL, DELIVER_TOOL, LIST_TOOL, REVISION};

/// 이 내장 예약 도구의 인자가 **실행 전에 확정적으로 틀렸는가**(`Invalid`): 호출 순서와 같은 구조·문법 검사(와 `create`의 `at`이 지금보다 앞인 시각)다.
/// 틀린 인자는 호출 기록·승인 카드 전에 거절되므로 CLI 재개의 확정 거절 표식(`CliRefusal`)을 남길 근거가 된다. 시각은 거절한 뒤에 다시 물어도 같은 답이다(시간은 앞으로만 간다).
pub(super) fn judged(
    tool: &str,
    args: &JsonPayload,
    default_timezone: &str,
    now: std::time::SystemTime,
) -> Option<bot_core::cli::CliRefusal> {
    let invalid = match tool {
        CREATE_TOOL => match Create::parse(args, default_timezone) {
            Ok(create) => create.spec(now).is_err(),
            Err(_) => true,
        },
        CANCEL_TOOL => parse_cancel(args).is_err(),
        LIST_TOOL => parse_list(args).is_err(),
        _ => false,
    };
    invalid.then_some(bot_core::cli::CliRefusal::Invalid)
}

/// 호스트 전용을 포함해 브로커가 직접 처리하는 예약 도구 ID인가.
pub fn is_builtin(tool: &str) -> bool {
    matches!(tool, CREATE_TOOL | LIST_TOOL | CANCEL_TOOL | DELIVER_TOOL)
}

pub fn create_tool() -> Tool {
    bot_core::builtin::CREATE.declaration()
}

pub fn list_tool() -> Tool {
    bot_core::builtin::LIST.declaration()
}

/// 다른 수신자를 지정한 생성: 사람이 수신자와 지시문을 확인해야 하는 **되돌릴 수 없는** 호출이다(정책과 무관하게 항상 승인). 다른 수신자에게 나가므로
/// private 기억을 읽은 실행이면 `private_recall` 이유가 더해진다.
pub fn create_tool_to() -> Tool {
    Tool {
        class: Risk::Irreversible,
        external: true,
        ..create_tool()
    }
}

pub fn cancel_tool() -> Tool {
    bot_core::builtin::CANCEL.declaration()
}

/// 승인·분류에 쓰는 선언(생성·취소). 목록과 호스트 전용은 승인이 없다.
pub fn mutating_tool(id: &str) -> Option<Tool> {
    match id {
        CREATE_TOOL => Some(create_tool()),
        CANCEL_TOOL => Some(cancel_tool()),
        _ => None,
    }
}

/// 호출 전에 확인할 공통 사실: 현재·고정 판본과 사람 요청자, 같은 이름의 스코프.
pub(super) struct Gatekeep {
    pub(super) ctx: RunContext,
    pub(super) current: std::sync::Arc<Snapshot>,
    pub(super) pinned: std::sync::Arc<Snapshot>,
}

/// 인자를 구조까지 읽은 호출이 하려는 일.
enum Plan {
    Create(Box<Create>),
    Cancel(ScheduleId),
    List,
}

/// 실제로 저장소에 보낼 일(시간에 기대는 검사를 마친 것).
enum Work {
    Create(ScheduleSpec),
    Cancel(ScheduleId),
    List,
}

impl BrokerService {
    /// 호출을 기록하기 전의 거절: 고정 단어 하나만 감사에 남긴다.
    pub(super) async fn refuse_schedule(
        &self,
        attempt: &Attempt,
        ctx: &RunContext,
        tool: &ToolId,
        word: &'static str,
    ) -> Result<CallResult, Error> {
        let _ = self
            .note(
                &attempt.profile,
                &ctx.principal,
                "schedule.refuse",
                tool,
                None,
                word,
            )
            .await;
        Err(Error::Denied)
    }

    pub(super) async fn gatekeep(
        &self,
        attempt: &Attempt,
        spec: &Tool,
    ) -> Result<Result<Gatekeep, &'static str>, Error> {
        let ctx = self.parts.ledger.context(attempt.clone()).await?;
        let current = self
            .parts
            .catalog
            .current(&attempt.profile)
            .ok_or(Error::Denied)?;
        let pinned = self
            .parts
            .catalog
            .lookup(&attempt.profile, &ctx.work.revision)
            .ok_or(Error::Missing)?;
        // 예약 주체는 인증된 사람이다.
        if !ctx.principal.is_human() {
            return Ok(Err("not_human"));
        }
        let scopes: &Scopes = &spec.scopes;
        if authorize(scopes, &ctx, &current, &pinned).is_err() {
            return Ok(Err("scope_missing"));
        }
        Ok(Ok(Gatekeep {
            ctx,
            current,
            pinned,
        }))
    }

    pub(super) async fn schedule_flow(
        &self,
        req: ToolCall,
        note: &mut Note,
    ) -> Result<CallResult, Error> {
        match req.tool.0.as_str() {
            LIST_TOOL | CREATE_TOOL | CANCEL_TOOL => self.schedule_call(req, note).await,
            // 호스트 전용: 모델이 부를 수 없다(존재도 알리지 않는다).
            _ => Err(Error::Missing),
        }
    }

    /// 이미 완료한 호출이면 그 결과. 도구와 인자 해시가 같을 때만이고(다르면 `Conflict`), 돌려주기 전에 현재 claim·판본을 다시 본다.
    /// `legacy`는 구판 봉인으로 만든 같은 인자의 지문이다: **완료한 호출**의 기록이 정확히 이 지문이면 영수증만 읽는다. 열려 있거나 승인된
    /// 호출·시작한 호출이 구판 지문이면 쓰지 않는다(`Conflict`) — 새 지문으로 다시 시작하거나 승인을 소비하지 않는다.
    async fn replayed_schedule(
        &self,
        attempt: &Attempt,
        call: &CallId,
        tool: &ToolId,
        sha256: [u8; 32],
        legacy: Option<[u8; 32]>,
        current: &Snapshot,
    ) -> Result<Option<JsonPayload>, Error> {
        let Some(record) = self
            .parts
            .ledger
            .lookup(attempt.clone(), call.clone())
            .await?
        else {
            return Ok(None);
        };
        let done = matches!(record.phase, CallPhase::Done(_));
        let same = record.args_sha256 == sha256 || (done && legacy == Some(record.args_sha256));
        if record.tool != *tool || !same {
            return Err(Error::Conflict);
        }
        let CallPhase::Done(result) = record.phase else {
            return Ok(None);
        };
        self.revision_current(attempt, current).await?;
        Ok(Some(result))
    }

    /// 저장소가 이 호출에 기록한 결과(완료 영수증). 읽지 못하면 처음 응답을 지어내지 않고 `Unavailable`이다(재전달이 같은 값을 받는다).
    pub(super) async fn recorded_receipt(&self, flow: &Flow) -> Result<JsonPayload, Error> {
        let record = self
            .parts
            .ledger
            .lookup(flow.attempt.clone(), flow.call.clone())
            .await?;
        match record.map(|record| record.phase) {
            Some(CallPhase::Done(result)) => Ok(result),
            _ => Err(Error::Unavailable),
        }
    }

    /// 모델이 지정한 다른 수신자를 호스트 등록부로 확정한다(없으면 `None` = 원래 대화). CLI·HTTP 입구는 원래 대화에만 답하고, 등록부가 없거나 등록되지 않은
    /// 수신자면 호출을 기록하거나 카드를 만들기 전에 거절한다.
    async fn resolve_destination(
        &self,
        attempt: &Attempt,
        ctx: &RunContext,
        tool: &ToolId,
        requested: Option<&DeliveryTarget>,
    ) -> Result<Option<DeliveryTarget>, Error> {
        let Some(requested) = requested else {
            return Ok(None);
        };
        if !matches!(
            ctx.conversation.kind,
            ConversationKind::Direct | ConversationKind::Channel
        ) {
            return self
                .refuse_schedule(attempt, ctx, tool, "destination_unsupported")
                .await
                .map(|_| None);
        }
        match self.route(&attempt.profile, requested) {
            Ok(target) => Ok(Some(target)),
            // 등록부를 붙이지 않았다
            Err(Error::Missing) => self
                .refuse_schedule(attempt, ctx, tool, "destination_unsupported")
                .await
                .map(|_| None),
            Err(Error::Denied) => self
                .refuse_schedule(attempt, ctx, tool, "destination_unregistered")
                .await
                .map(|_| None),
            Err(Error::Invalid) => {
                self.note_invalid(attempt, ctx, tool).await;
                Err(Error::Invalid)
            }
            Err(error) => Err(error),
        }
    }

    /// 인자가 틀렸다는 사실을 감사에 남긴다.
    async fn note_invalid(&self, attempt: &Attempt, ctx: &RunContext, tool: &ToolId) {
        let _ = self
            .note(
                &attempt.profile,
                &ctx.principal,
                "call.invalid",
                tool,
                None,
                "args",
            )
            .await;
    }

    /// 예약 도구 하나의 호출: 인가 → 인자 → 재전달 확인 → 호출 기록 → 승인(정책이 요구할 때) → 실행 → 결과.
    async fn schedule_call(&self, req: ToolCall, note: &mut Note) -> Result<CallResult, Error> {
        let ToolCall {
            attempt,
            call,
            tool,
            args,
        } = req;
        let spec = match tool.0.as_str() {
            CREATE_TOOL => create_tool(),
            CANCEL_TOOL => cancel_tool(),
            _ => list_tool(),
        };
        note.risk = Some(spec.class);
        note.revision = Some(REVISION.to_owned());
        let gate = match self.gatekeep(&attempt, &spec).await? {
            Ok(gate) => gate,
            Err(word) => {
                let ctx = self.parts.ledger.context(attempt.clone()).await?;
                return self.refuse_schedule(&attempt, &ctx, &tool, word).await;
            }
        };
        let Gatekeep {
            ctx,
            current,
            pinned,
        } = gate;
        // 구조만 읽고 봉인한다(해시가 승인에 묶인다). 생략한 시간대는 실행이 고정한 판본의 값으로 이때 채운다. 다른 수신자를 지정했으면 호스트
        // 등록부가 정규 경로로 확정한 값으로만 봉인하고, 그런 생성은 항상 소유자가 수신자와 지시문을 보고 승인한다(되돌릴 수 없는 호출).
        let parsed = match tool.0.as_str() {
            CREATE_TOOL => match Create::parse(&args, &pinned.timezone) {
                Ok(mut create) => {
                    let resolved = self
                        .resolve_destination(&attempt, &ctx, &tool, create.requested())
                        .await?;
                    let spec = if resolved.is_some() {
                        create_tool_to()
                    } else {
                        spec
                    };
                    create
                        .seal(resolved)
                        .map(|sealed| (Plan::Create(Box::new(create)), sealed, spec))
                }
                Err(reject) => Err(reject),
            },
            CANCEL_TOOL => parse_cancel(&args).map(|(id, sealed)| (Plan::Cancel(id), sealed, spec)),
            _ => parse_list(&args).map(|sealed| (Plan::List, sealed, spec)),
        };
        let (plan, sealed, spec) = match parsed {
            Ok(parts) => parts,
            Err(Reject::Invalid) => {
                self.note_invalid(&attempt, &ctx, &tool).await;
                return Err(Error::Invalid);
            }
        };
        // 이미 끝낸 호출의 재전달은 시간에 기대는 검사 없이 기록한 결과를 돌려준다(구판 지문으로 끝난 호출도 영수증만 읽는다).
        let legacy = (tool.0 == CREATE_TOOL)
            .then(|| legacy_create_sha256(&args, &pinned.timezone))
            .flatten();
        if let Some(done) = self
            .replayed_schedule(&attempt, &call, &tool, sealed.sha256, legacy, &current)
            .await?
        {
            return Ok(CallResult::Done(done));
        }
        let work = match plan {
            Plan::Create(create) => match create.spec(self.parts.clock.now()) {
                Ok(spec) => Work::Create(spec),
                Err(error) => {
                    self.note_invalid(&attempt, &ctx, &tool).await;
                    return Err(error);
                }
            },
            Plan::Cancel(id) => Work::Cancel(id),
            Plan::List => Work::List,
        };
        let record = self
            .parts
            .ledger
            .begin(attempt.clone(), call.clone(), tool.clone(), sealed.sha256)
            .await?;
        // 다른 수신자를 지정한 생성은 카드를 열기 직전과 승인을 소비하기 직전에 등록부로 그 수신자를 다시 본다.
        let route = match &work {
            Work::Create(spec) => spec.destination.clone(),
            _ => None,
        };
        let flow = Flow {
            // 소유자 전용 기능: OpenAI 승인 경로로 열지 않는다(요청자 본인이 승인자인 기존 규칙)
            origin: None,
            attempt,
            call,
            approver: approver_for(&ctx, &current),
            resolved: Resolved::new(tool.clone(), spec.clone()).ok_or(Error::Invalid)?,
            tool,
            ctx,
            snapshot: current,
            pinned,
            sealed,
            env: Vec::new(),
            memory: None,
            delivery: None,
            route,
        };
        let pointer = match &record.phase {
            CallPhase::Done(result) => {
                // 읽은 사이에 완료가 커밋됐다: 같은 결과다.
                self.still_current(&flow).await?;
                return Ok(CallResult::Done(result.clone()));
            }
            CallPhase::Denied => return Err(Error::Denied),
            CallPhase::Open { approval } => approval.clone(),
            CallPhase::Started { .. } | CallPhase::Held { .. } => {
                return Ok(CallResult::Held(flow.call.clone()));
            }
        };
        let _slot = self
            .limiter
            .admit(&flow.attempt.profile, &flow.ctx.principal)
            .map_err(|_| Self::limited())?;
        let class = classify(
            &spec,
            &flow.snapshot.policy.approve,
            flow.ctx.private_memory_read,
        );
        let approval = match self.gate(&flow, &class, pointer, None).await? {
            Gate::Go(approval) => approval,
            Gate::Return(result) => return Ok(result),
        };
        self.still_current_local(&flow)?;
        // 다른 수신자를 지정한 생성은 승인을 소비하기 직전에 등록부를 한 번 더 본다: 읽은 뒤 수신자가 빠졌거나 재지정됐으면 소비하지도 만들지도 않는다
        // (호출·승인은 그대로라 등록부가 돌아오면 같은 승인으로 이어진다).
        if let Some(target) = &flow.route {
            match self.route_state(&flow.attempt.profile, target) {
                Route::Same => {}
                Route::Gone => return self.route_gone(&flow, target).await,
                // 확인하지 못했다: 승인을 소비하지도 만들지도 않고 같은 호출을 다시 시도하게 한다
                Route::Unsure => return Err(Error::Unavailable),
            }
        }
        let start = StartCall {
            checked_revision: flow.snapshot.revision.clone(),
            risk: class.risk,
            tool_revision: Revision(REVISION.into()),
            attempt: flow.attempt.clone(),
            call: flow.call.clone(),
            approval: approval.map(|boxed| *boxed),
            private_recall: flow.ctx.private_memory_read,
            owner: format!(
                "schedule.{}.{}.{}",
                flow.attempt.run.0, flow.attempt.generation.0, flow.call.0
            ),
        };
        let result = match work {
            // 승인 소비·변경·호출 완료를 저장소가 한 커밋으로 한다. 영수증은 저장소가 호출에 기록한 값 그대로 돌려준다
            // (처음 응답과 재전달이 같은 모양이도록 한 곳에서 읽는다).
            Work::Create(spec) => {
                self.schedules.create(start, spec).await?;
                self.recorded_receipt(&flow).await?
            }
            Work::Cancel(id) => {
                self.schedules.cancel(start, id).await?;
                self.recorded_receipt(&flow).await?
            }
            // 목록은 읽기다: 일반 `start`로 시작하고 결과를 `finish`로 기록한다. 읽지 못하면 호출을 되돌려 재전달이 다시 읽게 한다.
            Work::List => {
                self.parts.ledger.start(start).await?;
                let listed = self
                    .schedules
                    .list(flow.attempt.clone(), flow.snapshot.revision.clone())
                    .await
                    .and_then(|list| render_list(&list).ok_or(Error::Unavailable));
                match listed {
                    Ok(payload) => {
                        self.finish(&flow, Outcome::Done(payload.clone())).await?;
                        payload
                    }
                    Err(error) => {
                        let _ = self.finish(&flow, Outcome::Released).await;
                        return Err(error);
                    }
                }
            }
        };
        // 돌려주기 전에 저장소 기준으로 현재 claim·판본을 다시 확인한다.
        self.still_current(&flow).await?;
        Ok(CallResult::Done(result))
    }
}

mod unavailable;
pub(super) use unavailable::NoSchedules;

#[cfg(test)]
mod tests;
