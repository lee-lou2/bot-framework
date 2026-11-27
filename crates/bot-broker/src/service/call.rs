//! 도구 호출 한 건의 진입·선언 결합·인가·거절 기록과 호출 스팬(`Broker::call` 본흐름).
//!
//! 전체 호출 순서와 불변식은 `service.rs` 머리말을 따르며, 승인 카드 개시·대조는 `gate`,
//! 실제 실행·복구는 `exec`, 기억 공유·예약 내장 도구는 `share`·`schedule`에 넘긴다.

use super::{
    BrokerService,
    authority::{Verdict, waivable},
    gate::Gate,
};
use crate::approver::{approver_for, openai};
use crate::{
    classify::{classify, risk_code},
    ports::*,
    resolve::{Resolved, effective_tool, find_tool, pinned_to_image, selected_env},
    scope::authorize,
};
use bot_core::{
    api::ApprovalBlock,
    command::*,
    id::*,
    ledger::*,
    port::Error,
    profile::{Snapshot, Tool},
    schedule::DeliveryTarget,
    state::{Risk, RunContext},
    trace::{SpanIds, SpanKind, SpanRecord, key, now_nano},
};
use std::sync::Arc;

/// 호출 하나를 처리하는 동안 들고 다니는 확정된 상태.
pub(super) struct Flow {
    pub attempt: Attempt,
    pub call: CallId,
    pub tool: ToolId,
    pub ctx: RunContext,
    /// 지금 활성인 판본(정책·역할·승인의 정책 판본).
    pub snapshot: Arc<Snapshot>,
    /// 실행이 고정한 판본.
    pub pinned: Arc<Snapshot>,
    /// 지금 판본이 지정한 승인자. 확인할 수 없으면 `None`.
    pub approver: Option<PrincipalId>,
    /// 이 실행이 인증된 OpenAI 호환 입구로 접수됐으면 그 승인 경로 판정(고정·현재 판본이 같을 때만 `Ok`). 아니면 `None`이다(기존 규칙).
    pub origin: Option<Result<openai::Origin, ApprovalBlock>>,
    pub resolved: Resolved,
    pub sealed: Sealed,
    /// 도구가 고른 환경 변수의 이름·값(고정·현재 판본이 같은 값을 준 것).
    pub env: Vec<(String, String)>,
    /// 기억 공유 호출이면 (기억 ID, 소유자가 볼 본문). 승인 카드가 이 본문을 보여 준다.
    pub memory: Option<(MemoryId, String)>,
    /// 예약 회신 전송이면 승인 문서(`DeliveryDraft::approval_json`). 승인 카드가 이 문서 전체를 보여 준다.
    pub delivery: Option<String>,
    /// 다른 수신자로 가는 호출(수신자를 지정한 생성·예약 회신 전송)이면 그 정규 수신자. 카드를 열기 직전에 현재 등록부로 다시 본다.
    pub route: Option<DeliveryTarget>,
    /// 승인 카드에 표시할 가림 인자 JSON(`mcp.add`처럼 비밀 값을 포함할 수 있는 호출). `None`이면 `sealed.canonical`을 쓴다.
    pub card_args: Option<String>,
}

/// 호출이 가리키는 도구와 판정에 쓰는 두 판본.
pub(super) struct Decl {
    pub resolved: Resolved,
    pub current: Arc<Snapshot>,
    pub pinned: Arc<Snapshot>,
    pub env: Vec<(String, String)>,
}

/// 스팬에 남길 분류. 호출 처리 중 알게 되는 대로 채운다.
#[derive(Default)]
pub(super) struct Note {
    pub risk: Option<Risk>,
    pub revision: Option<String>,
}

/// 도구가 선언한 자격 이름 중 이 호출이 쓸 수 없는 것이 있는가: 호스트 전용 이름이거나, 이 프로필이 허용한 목록(profile.yaml `secrets`)에 없다.
/// 그런 이름은 보관소에서 읽지 않는다(그 호출은 기록과 함께 거절한다). 돌려주는 것은 감사에 남기는 고정 낱말이다.
fn forbidden_secret(tool: &Tool, snapshot: &Snapshot) -> Option<&'static str> {
    if tool.secrets.values().any(crate::private::reserved_secret) {
        return Some("secret_reserved");
    }
    tool.secrets
        .values()
        .any(|name| !snapshot.secrets.contains(name))
        .then_some("secret_not_allowed")
}

fn error_class(error: &Error) -> &'static str {
    match error {
        Error::Invalid => "invalid",
        Error::Denied => "denied",
        Error::Conflict => "conflict",
        Error::Stale => "stale",
        Error::Missing => "missing",
        Error::Unavailable => "unavailable",
    }
}

impl BrokerService {
    /// 실행이 고정한 판본의 선언과 지금 활성 판본의 선언을 읽어 합친다. 지금 판본에서 사라진 도구(철회된 서버)와
    /// 설치되지 않은 서버는 거절한다. 설치 판본은 도구 판본에 묶여 승인에 들어간다.
    pub(super) fn declarations(
        &self,
        ctx: &RunContext,
        profile: &ProfileId,
        tool: &ToolId,
    ) -> Result<Decl, Error> {
        let catalog = &self.parts.catalog;
        let current = catalog.current(profile).ok_or(Error::Denied)?;
        let pinned = catalog
            .lookup(profile, &ctx.work.revision)
            .ok_or(Error::Missing)?;
        let pinned_tool = find_tool(&pinned, &ctx.agent, tool).ok_or(Error::Missing)?;
        let merged = if current.revision == pinned.revision {
            pinned_tool.clone()
        } else {
            let now = find_tool(&current, &ctx.agent, tool).ok_or(Error::Denied)?;
            effective_tool(pinned_tool, now)?
        };
        let image = self
            .parts
            .runner
            .installed(&merged.server)
            .ok_or(Error::Denied)?;
        let merged = pinned_to_image(merged, &image);
        // 고른 환경 변수의 값은 고정 판본과 현재 판본에서 같아야 한다. 다르면 이 실행은 새 판본으로 다시 시작해야 한다.
        let env = selected_env(&merged, &ctx.agent, &pinned, &current).ok_or(Error::Conflict)?;
        let resolved = Resolved::new(tool.clone(), merged).ok_or(Error::Invalid)?;
        Ok(Decl {
            resolved,
            current,
            pinned,
            env,
        })
    }

    pub(super) async fn call_flow(&self, req: ToolCall) -> Result<CallResult, Error> {
        let started = now_nano();
        let mut note = Note::default();
        let (attempt, call, tool) = (req.attempt.clone(), req.call.clone(), req.tool.clone());
        let result = self.call_inner(req, &mut note).await;
        self.end_call_span(started, &attempt, &call, &tool, note, &result);
        result
    }

    /// 호출 하나의 스팬을 남긴다(일반 호출과 호스트 전용 `release`가 같은 모양이다).
    pub(super) fn end_call_span(
        &self,
        started: u64,
        attempt: &Attempt,
        call: &CallId,
        tool: &ToolId,
        note: Note,
        result: &Result<CallResult, Error>,
    ) {
        let state = match result {
            Ok(CallResult::Done(_)) => "done",
            Ok(CallResult::Awaiting(_)) => "awaiting",
            Ok(CallResult::Held(_)) => "held",
            Ok(CallResult::Passed) => "passed",
            Err(Error::Denied) => "denied",
            Err(_) => "failed",
        };
        let mut span = SpanRecord::start(
            &attempt.profile,
            "broker.tool.call",
            SpanIds::attempt(attempt).child("tool", &call.0),
        )
        .kind(SpanKind::Internal)
        .started_at(started)
        .attr(key::GEN_AI_OPERATION_NAME, "execute_tool")
        .attr(key::GEN_AI_TOOL_TYPE, "function")
        .attr(key::GEN_AI_TOOL_NAME, tool.0.clone())
        .attr(key::GEN_AI_TOOL_CALL_ID, call.0.clone())
        .attr(key::BOT_ATTEMPT_GENERATION, attempt.generation.0)
        .attr(key::BOT_CALL_STATE, state);
        if let Some(risk) = note.risk {
            span = span.attr(key::BOT_TOOL_RISK, risk_code(risk));
        }
        if let Some(revision) = note.revision {
            span = span.attr(key::BOT_TOOL_REVISION, revision);
        }
        if let Err(error) = result {
            span = span.fail(error_class(error));
        }
        span.end(self.parts.tracer.as_ref());
    }

    async fn call_inner(&self, req: ToolCall, note: &mut Note) -> Result<CallResult, Error> {
        // 호스트 전용 호출 ID(예약 회신 전송)는 어떤 도구·인자로도 일반 경로가 선점할 수 없다(기록하지 않는다).
        if req.call.0 == bot_core::schedule::DELIVER_CALL {
            return self.refuse_reserved_call(&req.attempt, &req.tool).await;
        }
        // 기억 공유는 도구 서버를 거치지 않고 저장소가 승인과 공유를 한 번에 커밋한다.
        if req.tool.0 == super::share::SHARE_TOOL {
            return self.share_flow(req, note).await;
        }
        // 예약 내장 도구(생성·목록·취소)와 호스트 전용 ID는 도구 서버를 거치지 않는다.
        if super::schedule::is_builtin(&req.tool.0) {
            return self.schedule_flow(req, note).await;
        }
        // 슈퍼 관리자 전용 스킬·MCP 관리 도구(`skill.*`·`mcp.*`).
        if bot_core::builtin::manage_tool(&req.tool.0).is_some() {
            return self.manage_flow(req, note).await;
        }
        let ToolCall {
            attempt,
            call,
            tool,
            args,
        } = req;
        let ledger = &self.parts.ledger;
        let ctx = ledger.context(attempt.clone()).await?;
        let Decl {
            resolved,
            current: snapshot,
            pinned,
            env,
        } = self.declarations(&ctx, &attempt.profile, &tool)?;
        note.revision = Some(resolved.tool.revision.0.clone());
        // 이 호스트의 샌드박스가 실행할 수 없는 서버(상태 볼륨을 요구하는데 지원하지 않음)는 호출을 기록하기 전에 명시적으로 거절한다.
        if self.parts.runner.admit(&resolved.tool.server).is_err() {
            let _ = self
                .note(
                    &attempt.profile,
                    &ctx.principal,
                    "call.refuse",
                    &tool,
                    None,
                    "server_unsupported",
                )
                .await;
            return Err(Error::Denied);
        }
        let sealed = match self.parts.sealer.seal(&resolved.tool, &args) {
            Ok(sealed) => sealed,
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
        // 도구가 받을 자격이 이 프로필의 보관소에 없으면 새 호출은 기록하지도 승인 카드를 만들지도 않고 거절한다(실행할 수 없는 호출로 사람에게
        // 승인을 받지 않게). 새 호출의 순서는 «선언·서버 확인 → 인자 봉인 → 금지 이름 → 인가 → 속도 제한 → 자격 확인 → 기록»이다: 쓸 수 없는 이름·권한이
        // 철회된 요청자·한도를 넘은 호출은 보관소를 읽지 않고(그런 호출은 아래에서 기록과 함께 거절된다), 자격이 없는 호출도 속도 제한은 거친다(보관소를
        // 반복해서 읽게 만드는 호출을 막는다). 이미 기록한 호출(재전달·복구)은 기록대로 이어 간다: 자격이 사라졌어도 끝난 결과를 숨기지 않는다.
        // 읽지 못한 보관소는 «없음»이 아니다(실행기가 자기 규칙으로 다룬다).
        let mut held = None;
        // 원격 서버의 bearer처럼 호스트가 서버에 묶은 자격도 같은 자리에서 본다(없는 자격으로 사람에게 승인을 받지 않게). 원격 도구는 `secrets`를 선언하지 못하므로
        // 이 자격만 있는 호출도 이 점검을 거친다.
        let bound = self.parts.runner.server_secrets(&resolved.tool.server);
        if (!resolved.tool.secrets.is_empty() || !bound.is_empty())
            && forbidden_secret(&resolved.tool, &snapshot).is_none()
            && ledger
                .lookup(attempt.clone(), call.clone())
                .await?
                .is_none()
            && authorize(&resolved.tool.scopes, &ctx, &snapshot, &pinned).is_ok()
        {
            // 속도·동시성 자리는 아래 본 흐름이 그대로 이어 쓴다(두 번 세지 않는다). 한도를 넘으면 기록도 보관소 조회도 없이 돌려보낸다.
            held = Some(
                self.limiter
                    .admit(&attempt.profile, &ctx.principal)
                    .map_err(|_| Self::limited())?,
            );
            let mut names: Vec<_> = resolved.tool.secrets.values().cloned().collect();
            names.extend(bound);
            if matches!(
                self.parts
                    .runner
                    .admit_secrets(&attempt.profile, &names)
                    .await,
                Err(Error::Missing)
            ) {
                let _ = self
                    .note(
                        &attempt.profile,
                        &ctx.principal,
                        "call.refuse",
                        &tool,
                        None,
                        "secret_missing",
                    )
                    .await;
                return Err(Error::Denied);
            }
        }
        let record = ledger
            .begin(attempt.clone(), call.clone(), tool.clone(), sealed.sha256)
            .await?;
        // 기록된 도구와 정규 인자 해시가 이번 요청과 정확히 같을 때만 그 기록을 쓴다(저장소가 이미 걸렀더라도 한 번 더 본다).
        if record.tool != tool || record.args_sha256 != sealed.sha256 {
            return Err(Error::Conflict);
        }
        // OpenAI 출처 실행의 승인자는 접수 때 기록한 값이 아니라 원래 바인딩·HTTP 주체로 고정·현재 판본에서 다시 읽는다. 그 밖의 실행은 기존 규칙이다.
        let origin = openai::for_run(&ctx, &pinned, &snapshot);
        let approver = match &origin {
            Some(Ok(found)) => Some(found.route.person().clone()),
            Some(Err(_)) => None,
            None => approver_for(&ctx, &snapshot),
        };
        let flow = Flow {
            attempt,
            call,
            tool,
            ctx,
            snapshot,
            pinned,
            approver,
            origin,
            resolved,
            sealed,
            env,
            memory: None,
            delivery: None,
            route: None,
            card_args: None,
        };
        // 현재 권한: 저장된 결과·영수증을 돌려주기 전에도, 복구보다 먼저 확인한다(역할 철회 뒤 재호출).
        if let Err(denial) = authorize(
            &flow.resolved.tool.scopes,
            &flow.ctx,
            &flow.snapshot,
            &flow.pinned,
        ) {
            return match &record.phase {
                // 새로 시작할 호출은 거절로 닫는다. 이미 시작했거나 끝났거나 보류 중인 호출은 상태를 그대로 두고
                // (보류 증거를 지우지 않는다) 결과 없이 거절만 한다.
                CallPhase::Open { .. } => self.deny(&flow, denial.code()).await,
                CallPhase::Denied => Err(Error::Denied),
                _ => {
                    let _ = self
                        .note(
                            &flow.attempt.profile,
                            &flow.ctx.principal,
                            "call.replay",
                            &flow.tool,
                            Some(flow.sealed.sha256),
                            denial.code(),
                        )
                        .await;
                    Err(Error::Denied)
                }
            };
        }
        let pointer = match &record.phase {
            CallPhase::Done(result) => {
                // 결과를 돌려주기 직전에도 판정에 쓴 판본이 아직 활성인지(저장소 기준으로) 본다.
                self.still_current(&flow).await?;
                return Ok(CallResult::Done(result.clone()));
            }
            CallPhase::Denied => return Err(Error::Denied),
            CallPhase::Open { approval } => approval.clone(),
            CallPhase::Started { generation, .. } => {
                // 같은 세대가 이미 시작했다: 같은 호출이 동시에 두 번 들어왔다. 옛 세대의 것이면 복구한다.
                if *generation == flow.attempt.generation {
                    return Err(Error::Conflict);
                }
                match self.recover(&flow, &record.phase).await? {
                    super::exec::Recovered::Finished(result) => return Ok(result),
                    super::exec::Recovered::Open => None,
                }
            }
            CallPhase::Held { resolution, .. } => {
                if resolution.is_none() {
                    return Ok(CallResult::Held(flow.call.clone()));
                }
                match self.recover(&flow, &record.phase).await? {
                    super::exec::Recovered::Finished(result) => return Ok(result),
                    super::exec::Recovered::Open => None,
                }
            }
        };
        // 도구가 선언한 자격 이름은 호스트 전용이 아니고 이 프로필이 허용한 목록(profile.yaml secrets)에도 있어야 한다.
        if let Some(code) = forbidden_secret(&flow.resolved.tool, &flow.snapshot) {
            return self.deny(&flow, code).await;
        }
        // 속도·동시성. 자리는 실행이 끝날 때까지 쥔다(자격을 미리 확인한 새 호출은 그때 잡은 자리를 그대로 쓴다).
        let _slot = match held {
            Some(slot) => slot,
            None => self
                .limiter
                .admit(&flow.attempt.profile, &flow.ctx.principal)
                .map_err(|_| Self::limited())?,
        };
        let class = classify(
            &flow.resolved.tool,
            &flow.snapshot.policy.approve,
            flow.ctx.private_memory_read,
        );
        note.risk = Some(class.risk);
        // 사전 승인된 정적 직접 호출이나 지정한 사람의 증거로 승인될 수 있는 직접 호출이면 이 권한으로 시작한다
        let mut authority = match self.authority_for(&flow, &class, pointer.is_some()).await {
            Verdict::None => None,
            Verdict::Use(authority) => Some(authority),
            Verdict::Deny(why) => return self.deny(&flow, why).await,
        };
        let approval = match self
            .gate(&flow, &class, pointer, authority.as_ref())
            .await?
        {
            Gate::Go(approval) => approval.map(|boxed| *boxed),
            Gate::Return(result) => return Ok(result),
        };
        // 카드 승인을 소비하는 호출은 권한을 함께 쓰지 못한다: 사람 증거는 쓰지 않고, 사전 승인된 호출은 카드로 대신하지 않는다
        if approval.is_some() {
            match authority.take() {
                Some(bot_core::grant::Authority::Grant { .. }) => {
                    return self.deny(&flow, bot_core::grant::reason::STALE).await;
                }
                Some(bot_core::grant::Authority::Human { .. }) | None => {}
            }
        }
        debug_assert!(authority.is_none() || waivable(&class) || !class.needs_approval());
        self.execute(&flow, &class, approval, authority).await
    }

    /// 단독 감사 한 줄(상태 전이가 없는 거절·실패).
    pub(super) async fn note(
        &self,
        profile: &ProfileId,
        actor: &PrincipalId,
        action: &str,
        target: &ToolId,
        args_sha256: Option<[u8; 32]>,
        verdict: &str,
    ) -> Result<(), Error> {
        self.parts
            .audit
            .append(AuditEntry {
                profile: profile.clone(),
                actor: actor.0.clone(),
                action: action.into(),
                target: target.0.clone(),
                args_sha256,
                verdict: verdict.into(),
            })
            .await
            .map(|_| ())
    }

    /// `deny`와 같되 승인 경로 때문에 막았다는 닫힌 이유(`ApprovalBlock`)를 호출 기록에 함께 저장한다(감사 로그를 추측하지 않고 호스트가 저장된 판정으로 안내한다).
    /// 감사 낱말은 이유의 고정 낱말이다. 같은 호출 ID는 같은 거절을 받는다.
    pub(super) async fn deny_blocked<T>(
        &self,
        flow: &Flow,
        why: ApprovalBlock,
    ) -> Result<T, Error> {
        self.note(
            &flow.attempt.profile,
            &flow.ctx.principal,
            "call.deny",
            &flow.tool,
            Some(flow.sealed.sha256),
            why.code(),
        )
        .await?;
        self.parts
            .ledger
            .finish_blocked(
                FinishCall {
                    attempt: flow.attempt.clone(),
                    call: flow.call.clone(),
                    outcome: Outcome::Blocked,
                },
                why,
            )
            .await?;
        Err(Error::Denied)
    }

    /// 거절 이유를 감사에 남기고 호출을 닫는다. 같은 호출 ID는 같은 거절을 받는다.
    pub(super) async fn deny<T>(&self, flow: &Flow, code: &'static str) -> Result<T, Error> {
        self.note(
            &flow.attempt.profile,
            &flow.ctx.principal,
            "call.deny",
            &flow.tool,
            Some(flow.sealed.sha256),
            code,
        )
        .await?;
        self.parts
            .ledger
            .finish(FinishCall {
                attempt: flow.attempt.clone(),
                call: flow.call.clone(),
                outcome: Outcome::Blocked,
            })
            .await?;
        Err(Error::Denied)
    }
}
