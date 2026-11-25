//! 기억 공유(`memory.share`): 소유자가 본 본문 그대로를 승인에 묶는다.
//!
//! 순서: 소유자 본인의 DM·CLI인지 확인 → ID 한 건만 읽는다(`MemoryStore::preview`, 스캔 없음) → 카드와 승인 해시를 **이 한 번의 읽기**에서
//! 함께 만든다(`memory_share_hash`) → 소유자가 승인 → 저장소가 지금 본문으로 지문을 다시 계산해 호출의 해시와 대조하고
//! 승인 소비·가시성 변경·호출 완료를 한 커밋으로 처리한다(`MemoryStore::share`). 카드를 본 뒤 본문이 바뀌었으면 `Conflict`이고 승인은 소비되지 않는다.
//! 카드는 본문 전체를 보여 주며 자르거나 글을 해석해 ID를 얻지 않는다. 다 담을 수 없으면 공유하지 않는다.

use super::{
    BrokerService,
    call::{Flow, Note},
    gate::Gate,
};
use crate::{
    approver::approver_for,
    classify::{Class, Reason},
    json::parse,
    ports::Sealed,
    resolve::Resolved,
    scope::authorize,
    text::json_quote,
};
use bot_core::{
    command::{CallResult, JsonPayload, ToolCall, memory_share_hash},
    id::*,
    ledger::{CallPhase, StartCall},
    port::Error,
    profile::{Snapshot, Tool},
    state::{ConversationKind, Risk, RunContext, Visibility},
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// 내장 도구 ID. 에이전트가 이 스코프를 가져야 쓸 수 있다.
pub const SHARE_TOOL: &str = "memory.share";
pub const SHARE_SCOPE: &str = "memory.share";
pub const SHARE_REVISION: &str = "builtin-1";
const SCHEMA: &str = r#"{"additionalProperties":false,"properties":{"id":{"maxLength":128,"minLength":1,"type":"string"}},"required":["id"],"type":"object"}"#;

/// 내장 도구 선언(도구 서버 없음).
pub fn share_tool() -> Tool {
    Tool {
        server: "builtin".into(),
        revision: Revision(SHARE_REVISION.into()),
        class: Risk::Irreversible,
        scopes: BTreeSet::from([SHARE_SCOPE.to_owned()]),
        input: JsonPayload(SCHEMA.into()),
        env: BTreeSet::new(),
        secrets: BTreeMap::new(),
        egress: BTreeSet::new(),
        effects: false,
        external: false,
        destination: None,
        definition: None,
        mcp_name: None,
        timeout_secs: None,
    }
}

/// 승인 분류: 가시성을 넓히는 일은 되돌릴 수 없는 변경으로 본다.
pub fn share_class() -> Class {
    Class {
        risk: Risk::Irreversible,
        reasons: vec![Reason::Irreversible],
        external: false,
    }
}

/// 승인자: 소유자 본인의 1:1 대화(DM·CLI)에서 요청자가 곧 승인자이고 사람일 때만 있다.
pub(super) fn share_approver(ctx: &RunContext, current: &Snapshot) -> Option<PrincipalId> {
    matches!(
        ctx.conversation.kind,
        ConversationKind::Direct | ConversationKind::Cli
    )
    .then(|| approver_for(ctx, current))
    .flatten()
    .filter(|a| *a == ctx.principal && a.is_human())
}

/// 기억 공유의 인자가 **실행 전에 확정적으로 틀렸는가**(`Invalid`): 호출을 기록하기 전의 같은 검사다(CLI 재개의 확정 거절 표식을 남길 근거).
pub(super) fn judged(args: &JsonPayload) -> Option<bot_core::cli::CliRefusal> {
    memory_id(args)
        .is_err()
        .then_some(bot_core::cli::CliRefusal::Invalid)
}

/// `{"id": "<기억 ID>"}`만 받는다. 중복 키·다른 키·모양이 틀린 ID는 거절한다.
fn memory_id(args: &JsonPayload) -> Result<MemoryId, Error> {
    let Some(Value::Object(map)) = parse(&args.0) else {
        return Err(Error::Invalid);
    };
    match (map.len(), map.get("id").and_then(Value::as_str)) {
        (1, Some(id))
            if !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            Ok(MemoryId(id.to_owned()))
        }
        _ => Err(Error::Invalid),
    }
}

impl BrokerService {
    /// 호출을 기록하기 전의 거절: 고정 단어 하나만 감사에 남긴다.
    async fn refuse_share(
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
                "memory.share.refuse",
                tool,
                None,
                word,
            )
            .await;
        Err(Error::Denied)
    }

    /// 이 호출이 이미 공유를 마쳤다면 그 영수증. 도구가 `memory.share`이고 저장된 구조화 영수증 `{"id"}`가 요청의 기억 ID와
    /// 정확히 같을 때만 돌려준다(다른 도구나 다른 ID로 쓰인 호출 ID는 `Conflict`). 본문이 그 뒤에 고쳐졌거나 지워졌어도 같은
    /// 결과다. 돌려주기 전에 판정에 쓴 판본과 현재 claim을 저장소 기준으로 다시 확인한다(마지막 `await`).
    async fn replayed(
        &self,
        attempt: &Attempt,
        call: &CallId,
        id: &MemoryId,
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
        let CallPhase::Done(result) = &record.phase else {
            return Ok(None);
        };
        if record.tool.0 != SHARE_TOOL || receipt_id(result).as_deref() != Some(id.0.as_str()) {
            return Err(Error::Conflict);
        }
        self.revision_current(attempt, current).await?;
        Ok(Some(result.clone()))
    }

    pub(super) async fn share_flow(
        &self,
        req: ToolCall,
        note: &mut Note,
    ) -> Result<CallResult, Error> {
        let ToolCall {
            attempt,
            call,
            tool,
            args,
        } = req;
        let (ledger, memory) = (&self.parts.ledger, &self.parts.memory);
        note.risk = Some(Risk::Irreversible);
        note.revision = Some(SHARE_REVISION.to_owned());
        let ctx = ledger.context(attempt.clone()).await?;
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
        let spec = share_tool();
        // 소유자 본인의 DM·CLI에서만: 요청자가 곧 승인자이며 사람이어야 하고, 대화가 1:1이어야 한다.
        let approver = share_approver(&ctx, &current);
        if approver.is_none() {
            return self.refuse_share(&attempt, &ctx, &tool, "not_owner").await;
        }
        if authorize(&spec.scopes, &ctx, &current, &pinned).is_err() {
            return self
                .refuse_share(&attempt, &ctx, &tool, "scope_missing")
                .await;
        }
        let id = match memory_id(&args) {
            Ok(id) => id,
            Err(error) => {
                let _ = self
                    .note(
                        &attempt.profile,
                        &ctx.principal,
                        "call.invalid",
                        &tool,
                        None,
                        "args",
                    )
                    .await;
                return Err(error);
            }
        };
        // 현재 권한과 요청 ID를 확인한 뒤 본문을 읽기 전에 완료한 호출인지 본다: 재전달은 기억을 읽지도 다시 공유하지도 않는다.
        if let Some(receipt) = self.replayed(&attempt, &call, &id, &current).await? {
            return Ok(CallResult::Done(receipt));
        }
        // ID 한 건을 한 번만 읽는다. 카드의 본문과 승인 해시가 모두 이 읽기에서 나온다.
        let preview = match memory.preview(attempt.clone(), id.clone()).await {
            Ok(preview) => preview,
            Err(error) => {
                // 경쟁: 다른 재전달이 먼저 완료했다면(그 사이 본문이 지워졌을 수도 있다) 같은 엄격한 조건으로 영수증을 본다.
                if let Some(receipt) = self.replayed(&attempt, &call, &id, &current).await? {
                    return Ok(CallResult::Done(receipt));
                }
                return match error {
                    Error::Missing | Error::Denied => {
                        self.refuse_share(&attempt, &ctx, &tool, "memory_unavailable")
                            .await
                    }
                    error => Err(error),
                };
            }
        };
        // 미리보기가 요청 트리의 private 플래그를 올렸을 수 있으니 시작 판정에 쓸 맥락을 다시 읽는다.
        let ctx = ledger.context(attempt.clone()).await?;
        let sha256 = memory_share_hash(&id, &preview.sha256);
        let sealed = Sealed {
            canonical: format!("{{\"id\":{}}}", json_quote(&id.0)),
            sha256,
            destination: None,
        };
        let record = match ledger
            .begin(attempt.clone(), call.clone(), tool.clone(), sha256)
            .await
        {
            Ok(record) => record,
            Err(error) => {
                // 경쟁: 완료가 먼저 커밋되면 본문이 달라 해시가 맞지 않을 수 있다.
                if let Some(receipt) = self.replayed(&attempt, &call, &id, &current).await? {
                    return Ok(CallResult::Done(receipt));
                }
                return Err(error);
            }
        };
        // 비공개 여부는 새 공유에만 따진다.
        if matches!(record.phase, CallPhase::Open { .. })
            && preview.visibility != Visibility::Private
        {
            return Err(Error::Conflict); // 이미 프로필에 공유됐다
        }
        let flow = Flow {
            // 소유자 전용 기능: OpenAI 승인 경로로 열지 않는다(요청자 본인이 승인자인 기존 규칙)
            origin: None,
            attempt,
            call,
            tool,
            ctx,
            snapshot: current,
            pinned,
            approver,
            resolved: Resolved::new(ToolId(SHARE_TOOL.into()), spec).ok_or(Error::Invalid)?,
            sealed,
            env: Vec::new(),
            memory: Some((id.clone(), preview.text)),
            delivery: None,
            route: None,
        };
        let pointer = match &record.phase {
            CallPhase::Done(_) => {
                // 읽은 사이에 완료가 커밋됐다: 같은 엄격한 조건으로 영수증을 돌려준다(다시 공유하지 않는다).
                return match self
                    .replayed(&flow.attempt, &flow.call, &id, &flow.snapshot)
                    .await?
                {
                    Some(receipt) => Ok(CallResult::Done(receipt)),
                    None => Err(Error::Conflict),
                };
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
        let class = share_class();
        let approval = match self.gate(&flow, &class, pointer, None).await? {
            Gate::Go(approval) => approval,
            Gate::Return(result) => return Ok(result),
        };
        self.still_current_local(&flow)?;
        let start = StartCall {
            checked_revision: flow.snapshot.revision.clone(),
            risk: class.risk,
            tool_revision: Revision(SHARE_REVISION.into()),
            attempt: flow.attempt.clone(),
            call: flow.call.clone(),
            approval: approval.map(|boxed| *boxed),
            private_recall: flow.ctx.private_memory_read,
            owner: format!(
                "share.{}.{}.{}",
                flow.attempt.run.0, flow.attempt.generation.0, flow.call.0
            ),
        };
        // 승인 소비·공유·호출 완료를 저장소가 한 커밋으로 한다. 본문이 바뀌었으면 `Conflict`이고 승인은 그대로다.
        let shared = memory.share(start, id).await?;
        let result = JsonPayload(format!("{{\"id\":{}}}", json_quote(&shared.0)));
        self.still_current(&flow).await?;
        Ok(CallResult::Done(result))
    }
}

/// 저장된 영수증 `{"id": "<기억 ID>"}`에서 ID. 다른 모양(키가 더 있거나 문자열이 아님)은 `None`이다.
fn receipt_id(result: &JsonPayload) -> Option<String> {
    let Value::Object(map) = parse(&result.0)? else {
        return None;
    };
    match (map.len(), map.get("id")) {
        (1, Some(Value::String(id))) => Some(id.clone()),
        _ => None,
    }
}
