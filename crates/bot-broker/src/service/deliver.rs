//! 예약 실행의 최종 회신을 **다른 수신자**에게 보내는 호스트 전용 입구(`schedule.deliver`, `ScheduleDelivery::release`).
//!
//! 모델은 이 도구를 볼 수도 부를 수도 없다(목록에 없고, 호출 ID `schedule-deliver`도 일반 경로가 선점하지 못한다 — `call.rs`).
//! 순서: 이미 끝난 전송이면 전용 영수증(`ScheduledReplies::receipt`) → 사람 요청자·스코프(현재·고정 판본) → 워커가 동결한 초안 한 건(`ScheduledReplies::draft`) →
//! 승인 문서와 해시를 **이 한 번의 읽기**에서 함께 만든다(`approval_json`, 카드·원장·저장소가 같은 바이트의 SHA-256을 쓴다) → 카드에 전부
//! 담기는지 미리 확인(못 담으면 일부만 보여 주고 승인받지 않는다: 기록 없이 거절) → 호출 기록 → 승인(되돌릴 수 없는 호출이라 항상) →
//! 저장소가 승인 소비·목적지 outbox·run 완료를 한 커밋으로 쓴다(`ScheduledReplies::enqueue`). 그 커밋이 run을 끝내 리스를 놓으므로 그 뒤의 일반 원장
//! 조회(`context`·`lookup`·`active_revision`)는 `Stale`이다: 정상 커밋 직후와 응답을 잃은 재시도는 일반 조회 대신 전용 읽기로 같은 영수증을 돌려준다.
//! 승인자는 예약을 만든 사람이자 실행의 요청자 본인이다. 카드는 본인 전용 화면에만 나가고 공개 대화에는 보이지 않는다(게이트웨이·관리 화면 몫).
//! 승인 뒤 초안이 바뀌었거나 활성 집합·수신 경로가 달라졌다면 저장소가 `Conflict`로 닫고 발송은 0건이다(승인은 소비되지 않는다).

use super::{
    BrokerService,
    call::{Flow, Note},
    gate::Gate,
    route::Route,
    schedule::DELIVER_TOOL,
};
use crate::{
    approval::delivery_lines,
    approver::approver_for,
    canon::sha256,
    classify::{Class, classify},
    ports::Sealed,
    resolve::Resolved,
};
use bot_core::{
    command::{CallResult, JsonPayload},
    id::*,
    ledger::{CallPhase, StartCall},
    port::{Error, Reply},
    profile::{Snapshot, Tool},
    schedule::{
        DELIVER_CALL, DELIVER_REVISION, DeliveryDraft, ScheduleDelivery, ScheduledReplies,
        UnsentReason,
    },
    state::{Risk, RunContext},
};
use std::collections::{BTreeMap, BTreeSet};

const SCHEMA: &str = r#"{"additionalProperties":false,"properties":{},"type":"object"}"#;

/// 내장 도구 선언(도구 서버 없음, 인자 없음). 스코프는 예약을 만들 때 필요했던 `schedule.create`다: 그 권한이 지금도 있어야 보낸다.
pub fn deliver_tool() -> Tool {
    Tool {
        server: "builtin".into(),
        revision: Revision(DELIVER_REVISION.into()),
        class: Risk::Irreversible,
        scopes: BTreeSet::from([super::schedule::CREATE_TOOL.to_owned()]),
        input: JsonPayload(SCHEMA.into()),
        description: None,
        env: BTreeSet::new(),
        secrets: BTreeMap::new(),
        egress: BTreeSet::new(),
        effects: false,
        // 다른 수신자에게 나가는 일이다: private 기억을 읽은 실행이면 승인 이유에 `private_recall`이 더해진다.
        external: true,
        destination: None,
        definition: None,
        mcp_name: None,
        timeout_secs: None,
    }
}

/// 승인 분류. 다른 수신자로 나가는 일은 되돌릴 수 없는 호출이라 승인이 항상 필요하다.
pub(super) fn deliver_class(current: &Snapshot, ctx: &RunContext) -> Class {
    classify(
        &deliver_tool(),
        &current.policy.approve,
        ctx.private_memory_read,
    )
}

/// 승인자: 실행의 요청자 본인이고 사람이며 지금 판본의 멤버일 때만 있다(예약을 만든 사람이 곧 승인자다).
pub(super) fn deliver_approver(ctx: &RunContext, current: &Snapshot) -> Option<PrincipalId> {
    approver_for(ctx, current).filter(|approver| *approver == ctx.principal && approver.is_human())
}

/// 동결한 초안을 읽은 결과: 초안, 또는 그 사이 수신자가 빠져 이미 끝낸 발생의 결과.
enum Drafted {
    Draft(DeliveryDraft),
    Finished(CallResult),
}

/// 초안 저장소를 붙이지 않은 브로커: 초안을 읽지도 보내지도 못한다.
pub(super) struct NoReplies;

impl ScheduledReplies for NoReplies {
    fn draft(
        &self,
        _: Attempt,
        _: Option<bot_core::command::Finish>,
    ) -> Reply<'_, Option<bot_core::schedule::DeliveryDraft>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn enqueue(&self, _: StartCall) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

impl ScheduleDelivery for BrokerService {
    fn release(&self, attempt: Attempt) -> Reply<'_, CallResult> {
        Box::pin(self.release_flow(attempt))
    }
}

impl BrokerService {
    /// 호스트 전용 호출 하나(스팬은 일반 호출과 같은 모양으로 남긴다).
    async fn release_flow(&self, attempt: Attempt) -> Result<CallResult, Error> {
        let started = bot_core::trace::now_nano();
        let mut note = Note::default();
        let (call, tool) = (CallId(DELIVER_CALL.into()), ToolId(DELIVER_TOOL.into()));
        let result = self.deliver_flow(attempt.clone(), &mut note).await;
        self.end_call_span(started, &attempt, &call, &tool, note, &result);
        result
    }

    async fn deliver_flow(&self, attempt: Attempt, note: &mut Note) -> Result<CallResult, Error> {
        let (call, tool, spec) = (
            CallId(DELIVER_CALL.into()),
            ToolId(DELIVER_TOOL.into()),
            deliver_tool(),
        );
        note.risk = Some(Risk::Irreversible);
        note.revision = Some(DELIVER_REVISION.to_owned());
        // 이미 끝난 전송(정상 커밋 직후·응답을 잃은 재시도): 저장소가 run을 끝내 일반 원장 조회는 `Stale`이라 전용 읽기를 먼저 한다.
        // 등록 전에는 `None`이라 아래로 이어지고(실행 권한은 일반 claim 검사가 확인한다), 등록 뒤 다른 claim·바뀐 집합은 `Stale`이다.
        if let Some(receipt) = self.replies.receipt(attempt.clone()).await? {
            return Ok(CallResult::Done(receipt));
        }
        let gate = match self.gatekeep(&attempt, &spec).await? {
            Ok(gate) => gate,
            Err(word) => {
                let ctx = self.parts.ledger.context(attempt.clone()).await?;
                return self.refuse_schedule(&attempt, &ctx, &tool, word).await;
            }
        };
        let (ctx, current, pinned) = (gate.ctx, gate.current, gate.pinned);
        // 예약을 만든 본인(사람)만 승인할 수 있다. 확인할 수 없으면 기록 없이 거절한다.
        let Some(approver) = deliver_approver(&ctx, &current) else {
            return self
                .refuse_schedule(&attempt, &ctx, &tool, "not_owner")
                .await;
        };
        // 동결한 초안 한 건. 없으면(일반 실행·원래 대화로 보내는 답·첨부 없는 조용한 성공) 보낼 것이 없다.
        let draft = match self.draft_or_unrouted(&attempt).await? {
            Drafted::Draft(draft) => draft,
            Drafted::Finished(result) => return Ok(result),
        };
        // 수신자가 지금 호스트 등록부에서도 같은 경로여야 이어 간다: 빠졌거나 재지정됐으면 이 발생을 «미발송»으로 끝내고(원래 대화 안내 한 건, 카드·기록·발송 0),
        // 확인하지 못하면(일시 오류·등록부 미연결) 아무것도 하지 않고 같은 발생을 다시 시도한다.
        // 소유자가 이미 거절·철회한 발생이면 등록부 상태와 무관하게 «거절»이다(기록된 결정이 먼저다).
        match self.route_state(&attempt.profile, &draft.target) {
            Route::Same => {}
            route => return self.unsettled_route(&attempt, &draft.target, route).await,
        }
        // 카드에 보일 문서와 승인 해시가 같은 바이트에서 나온다. 읽을 수 없는 답(첨부 형식 등)이 하나라도 있으면 묶음 전체를 거절한다.
        let Ok(document) = draft.approval_json() else {
            return self
                .refuse_schedule(&attempt, &ctx, &tool, "card_unsupported")
                .await;
        };
        let hash = sha256(&document);
        let class = deliver_class(&current, &ctx);
        // 카드의 실제 줄 한도(머리줄 포함)를 넘으면 일부만 보여 주고 승인받지 않는다: 호출을 기록하기 전에 거절한다.
        if delivery_lines(&class, &ctx.principal, &approver, &document).is_none() {
            return self
                .refuse_schedule(&attempt, &ctx, &tool, "card_unsupported")
                .await;
        }
        let record = self
            .parts
            .ledger
            .begin(attempt.clone(), call.clone(), tool.clone(), hash)
            .await?;
        // 기록된 도구와 승인 문서 해시가 정확히 같을 때만 쓴다(초안이 달라졌다면 이 호출은 이어갈 수 없다).
        if record.tool != tool || record.args_sha256 != hash {
            return Err(Error::Conflict);
        }
        let flow = Flow {
            // 소유자 전용 기능: OpenAI 승인 경로로 열지 않는다(요청자 본인이 승인자인 기존 규칙)
            origin: None,
            attempt,
            call,
            resolved: Resolved::new(tool.clone(), spec).ok_or(Error::Invalid)?,
            tool,
            ctx,
            snapshot: current,
            pinned,
            approver: Some(approver),
            sealed: Sealed {
                canonical: document.clone(),
                sha256: hash,
                destination: None,
            },
            env: Vec::new(),
            memory: None,
            delivery: Some(document),
            route: Some(draft.target.clone()),
            card_args: None,
        };
        let pointer = match &record.phase {
            CallPhase::Done(result) => {
                // 읽은 사이에 완료가 커밋됐다: 같은 영수증이다.
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
            .map_err(|_| Error::Unavailable)?;
        let approval = match self.gate(&flow, &class, pointer, None).await? {
            Gate::Go(approval) => approval,
            Gate::Return(result) => return Ok(result),
        };
        // 되돌릴 수 없는 호출은 승인 없이 보내지 않는다(분류가 항상 승인을 요구하지만 한 번 더 막는다).
        let Some(approval) = approval else {
            return self.deny(&flow, "approval_missing").await;
        };
        self.still_current_local(&flow)?;
        // 승인을 소비하기 직전에 등록부를 다시 본다: 승인한 뒤 수신자가 빠졌거나 재지정됐으면 승인을 소비하지도 보내지도 않고 이 발생을 «미발송»으로 끝낸다
        // (옛 카드는 저장소가 같은 커밋에서 무효로 한다. 등록부가 돌아와도 그 승인으로 다시 보내지 않는다). 확인하지 못하면 같은 발생을 다시 시도한다.
        // (그 사이 소유자가 철회했다면 등록부 상태와 무관하게 «거절»이다.)
        match self.route_state(&flow.attempt.profile, &draft.target) {
            Route::Same => {}
            route => {
                return self
                    .unsettled_route(&flow.attempt, &draft.target, route)
                    .await;
            }
        }
        let start = StartCall {
            checked_revision: flow.snapshot.revision.clone(),
            risk: class.risk,
            tool_revision: Revision(DELIVER_REVISION.into()),
            attempt: flow.attempt.clone(),
            call: flow.call.clone(),
            approval: Some(*approval),
            private_recall: flow.ctx.private_memory_read,
            owner: format!(
                "deliver.{}.{}",
                flow.attempt.run.0, flow.attempt.generation.0
            ),
        };
        // 승인 소비·목적지 outbox·run 완료를 저장소가 한 커밋으로 쓴다(승인 뒤 초안·활성 집합이 달라졌으면 `Conflict`, 승인은 그대로).
        if let Err(error) = self.replies.enqueue(start).await {
            // 소비하려는 순간 소유자가 이미 철회했다면 저장소가 거절한다(`Denied`, 활성 집합이 함께 바뀌었으면 `Conflict`): 소비도 발송도 없었고 기록된 결정이
            // 먼저이니 «거절»로 끝낸다(재호출도 같은 영수증). 소유자 결정이 없는 거절(만료·대체된 승인 등)은 그대로다.
            if matches!(error, Error::Denied | Error::Conflict)
                && self.owner_rejected(&flow.attempt).await?
            {
                return self
                    .finish_unsent(&flow.attempt, &draft.target, UnsentReason::Rejected)
                    .await;
            }
            return Err(error);
        }
        // 저장소가 run을 끝냈다: 일반 원장 조회는 더 못 하니 전용 읽기로 영수증을 읽는다(커밋했는데 읽지 못하면 재시도가 같은 값을 받는다).
        let receipt = self
            .replies
            .receipt(flow.attempt.clone())
            .await?
            .ok_or(Error::Unavailable)?;
        Ok(CallResult::Done(receipt))
    }

    /// 동결한 초안을 읽는다. 활성 집합이 바뀌어 옛 집합에서 동결한 초안이면 저장소가 본문을 내주지 않고 `Conflict`인데, 그 `Conflict`가 «그 사이 수신자가
    /// 등록부에서 빠졌다»를 가리지 않게 고정 수신자만 먼저 읽어 현재 등록부로 가른다: 빠졌으면 이 발생을 미발송으로 끝내고, 아니면 본문을 보내지 않는
    /// 그대로 `Conflict`다(예전 승인을 소비하는 펜스는 그대로다).
    async fn draft_or_unrouted(&self, attempt: &Attempt) -> Result<Drafted, Error> {
        match self.replies.draft(attempt.clone(), None).await {
            Ok(Some(draft)) => Ok(Drafted::Draft(draft)),
            Ok(None) => Err(Error::Missing),
            Err(Error::Conflict) => {
                let Some(target) = self.replies.target(attempt.clone()).await? else {
                    return Err(Error::Conflict);
                };
                // 소유자가 이미 거절·철회했다면 «보낼 수 없는 초안»도 «빠진 수신자»도 아니다: 거절이다
                if self.owner_rejected(attempt).await? {
                    return self
                        .finish_unsent(attempt, &target, UnsentReason::Rejected)
                        .await
                        .map(Drafted::Finished);
                }
                match self.route_state(&attempt.profile, &target) {
                    Route::Gone => self
                        .finish_unsent(attempt, &target, UnsentReason::Unrouted)
                        .await
                        .map(Drafted::Finished),
                    Route::Same => Err(Error::Conflict),
                    Route::Unsure => Err(Error::Unavailable),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 일반 호출 경로가 호스트 전용 호출 ID를 쓰려 할 때의 거절: 어떤 도구·인자로도 기록하지 않는다(선점 방지).
    pub(super) async fn refuse_reserved_call(
        &self,
        attempt: &Attempt,
        tool: &ToolId,
    ) -> Result<CallResult, Error> {
        let ctx = self.parts.ledger.context(attempt.clone()).await?;
        let _ = self
            .note(
                &attempt.profile,
                &ctx.principal,
                "call.refuse",
                tool,
                None,
                "reserved_call",
            )
            .await;
        Err(Error::Denied)
    }
}
