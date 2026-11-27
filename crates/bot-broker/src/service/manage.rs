//! 슈퍼 관리자 전용 런타임 스킬·MCP 관리 도구(`skill.*`·`mcp.*`) 브로커 처리.
//!
//! - 모델이 호스트 파일·셸·비밀에 직접 닿지 않도록 모든 관리 호출은 이 관문을 지난다.
//! - 호출마다 고정 판본과 현재 판본 **모두**에서 슈퍼 관리자(`super_admins`)와 스코프를 확인한다.
//! - `mcp.add`가 승인 카드를 열 때 `secret_value`·`secret_values` 원문은 카드에 넣지 않고 가린다.
//! - 변경이 새 판본을 즉시 활성화하면(`ManageOutcome::activated`), 완료 직전 카탈로그와 저장소의 활성 판본이 그 새 판본인지 대조한다.

use super::{
    BrokerService,
    call::{Flow, Note},
    gate::Gate,
};
use crate::{
    approver::{approver_for, openai},
    canon::canonicalize,
    classify::classify,
    json::parse,
    resolve::Resolved,
    scope::authorize,
    text::json_quote,
};
use bot_core::{
    builtin::{MCP_ADD_TOOL, REVISION as BUILTIN_REVISION, manage_tool},
    command::{CallResult, JsonPayload, ToolCall},
    id::*,
    ledger::{CallPhase, Outcome, StartCall},
    manage::{ManageCall, ManageError},
    port::Error,
    state::Risk,
};
use serde_json::Value;

/// 승인 카드에 비밀 평문이 남지 않도록 `mcp.add` 인자의 `secret_value`·`secret_values` 값만 가린다.
/// 승인 해시(`sealed.sha256`)는 여전히 원본 정규 인자에 묶인다.
fn scrub_card_args(tool: &str, canonical: &str) -> Option<String> {
    if tool != MCP_ADD_TOOL {
        return None;
    }
    let Value::Object(mut map) = parse(canonical)? else {
        return None;
    };
    let mut changed = false;
    if map.contains_key("secret_value") {
        map.insert("secret_value".into(), Value::String("[REDACTED]".into()));
        changed = true;
    }
    if let Some(Value::Object(secrets)) = map.get_mut("secret_values") {
        for value in secrets.values_mut() {
            *value = Value::String("[REDACTED]".into());
        }
        changed = true;
    }
    changed.then(|| canonicalize(&Value::Object(map)))?
}

fn rejected_payload(code: &'static str, message: &str) -> JsonPayload {
    JsonPayload(format!(
        "{{\"code\":{},\"error\":{},\"ok\":false}}",
        json_quote(code),
        json_quote(message)
    ))
}

impl BrokerService {
    pub(super) async fn manage_flow(
        &self,
        req: ToolCall,
        note: &mut Note,
    ) -> Result<CallResult, Error> {
        let Some(spec) = manage_tool(&req.tool.0) else {
            return Err(Error::Missing);
        };
        let ToolCall {
            attempt,
            call,
            tool,
            args,
        } = req;
        let decl = spec.declaration();
        note.risk = Some(decl.class);
        note.revision = Some(BUILTIN_REVISION.to_owned());

        let ledger = &self.parts.ledger;
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

        let sealed = match self.parts.sealer.seal(&decl, &args) {
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

        let record = ledger
            .begin(attempt.clone(), call.clone(), tool.clone(), sealed.sha256)
            .await?;
        if record.tool != tool || record.args_sha256 != sealed.sha256 {
            return Err(Error::Conflict);
        }

        let origin = openai::for_run(&ctx, &pinned, &current);
        let approver = match &origin {
            Some(Ok(found)) => Some(found.route.person().clone()),
            Some(Err(_)) => None,
            None => approver_for(&ctx, &current),
        };
        let card_args = scrub_card_args(&tool.0, &sealed.canonical);
        let flow = Flow {
            attempt,
            call,
            resolved: Resolved::new(tool.clone(), decl).ok_or(Error::Invalid)?,
            tool,
            ctx,
            snapshot: current,
            pinned,
            approver,
            origin,
            sealed,
            env: Vec::new(),
            memory: None,
            delivery: None,
            route: None,
            card_args,
        };

        // 슈퍼 관리자 확인은 재전달·복구보다 먼저, 고정 판본과 현재 판본 모두에서 본다.
        if !flow.snapshot.is_super_admin(&flow.ctx.principal)
            || !flow.pinned.is_super_admin(&flow.ctx.principal)
        {
            return self
                .deny_phase(&flow, &record.phase, "not_super_admin")
                .await;
        }
        if let Err(denial) = authorize(
            &flow.resolved.tool.scopes,
            &flow.ctx,
            &flow.snapshot,
            &flow.pinned,
        ) {
            return self.deny_phase(&flow, &record.phase, denial.code()).await;
        }

        let pointer = match &record.phase {
            CallPhase::Done(result) => {
                self.still_current(&flow).await?;
                return Ok(CallResult::Done(result.clone()));
            }
            CallPhase::Denied => return Err(Error::Denied),
            CallPhase::Open { approval } => approval.clone(),
            CallPhase::Started { generation, .. } => {
                if *generation == flow.attempt.generation {
                    return Err(Error::Conflict);
                }
                if flow.resolved.tool.class == Risk::Read {
                    self.finish(&flow, Outcome::Released).await?;
                    None
                } else {
                    self.finish(&flow, Outcome::Held("manage_interrupted".into()))
                        .await?;
                    return Ok(CallResult::Held(flow.call.clone()));
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

        let Some(manager) = self.manager.as_ref() else {
            return Err(Error::Unavailable);
        };
        let _slot = self
            .limiter
            .admit(&flow.attempt.profile, &flow.ctx.principal)
            .map_err(|_| Self::limited())?;
        let class = classify(
            &flow.resolved.tool,
            &flow.snapshot.policy.approve,
            flow.ctx.private_memory_read,
        );
        note.risk = Some(class.risk);
        let approval = match self.gate(&flow, &class, pointer, None).await? {
            Gate::Go(approval) => approval.map(|boxed| *boxed),
            Gate::Return(result) => return Ok(result),
        };

        self.still_current_local(&flow)?;
        let owner = format!(
            "manage.{}.{}.{}",
            flow.attempt.run.0, flow.attempt.generation.0, flow.call.0
        );
        self.parts
            .ledger
            .start(StartCall {
                checked_revision: flow.snapshot.revision.clone(),
                risk: class.risk,
                tool_revision: flow.resolved.tool.revision.clone(),
                attempt: flow.attempt.clone(),
                call: flow.call.clone(),
                approval,
                private_recall: flow.ctx.private_memory_read,
                owner,
            })
            .await?;

        let outcome = manager
            .execute(ManageCall {
                profile: flow.attempt.profile.clone(),
                agent: flow.ctx.agent.clone(),
                principal: flow.ctx.principal.clone(),
                tool: flow.tool.0.clone(),
                args: JsonPayload(flow.sealed.canonical.clone()),
            })
            .await;

        match outcome {
            Ok(Ok(done)) => {
                let action = if class.risk == Risk::Read {
                    "manage.inspect"
                } else {
                    "manage.apply"
                };
                self.note(
                    &flow.attempt.profile,
                    &flow.ctx.principal,
                    action,
                    &flow.tool,
                    Some(flow.sealed.sha256),
                    "ok",
                )
                .await?;
                self.finish(&flow, Outcome::Done(done.payload.clone()))
                    .await?;
                match done.activated {
                    Some(new_rev) => {
                        self.verify_activated(&flow.attempt, &new_rev).await?;
                    }
                    None => {
                        self.still_current(&flow).await?;
                    }
                }
                Ok(CallResult::Done(done.payload))
            }
            Ok(Err(ManageError::Rejected { code, message })) => {
                self.note(
                    &flow.attempt.profile,
                    &flow.ctx.principal,
                    "manage.reject",
                    &flow.tool,
                    Some(flow.sealed.sha256),
                    code,
                )
                .await?;
                let payload = rejected_payload(code, &message);
                self.finish(&flow, Outcome::Done(payload.clone())).await?;
                self.still_current(&flow).await?;
                Ok(CallResult::Done(payload))
            }
            Ok(Err(ManageError::Conflict)) => {
                self.finish(&flow, Outcome::Released).await?;
                Err(Error::Conflict)
            }
            Ok(Err(ManageError::Unavailable(code))) => {
                self.note(
                    &flow.attempt.profile,
                    &flow.ctx.principal,
                    "manage.fail",
                    &flow.tool,
                    Some(flow.sealed.sha256),
                    code,
                )
                .await?;
                if class.risk == Risk::Read {
                    self.finish(&flow, Outcome::Released).await?;
                    Err(Error::Unavailable)
                } else {
                    self.finish(&flow, Outcome::Held(code.into())).await?;
                    Ok(CallResult::Held(flow.call.clone()))
                }
            }
            Err(error) => {
                if class.risk == Risk::Read {
                    self.finish(&flow, Outcome::Released).await?;
                    Err(error)
                } else {
                    self.finish(&flow, Outcome::Held("manage_error".into()))
                        .await?;
                    Ok(CallResult::Held(flow.call.clone()))
                }
            }
        }
    }

    async fn deny_phase(
        &self,
        flow: &Flow,
        phase: &CallPhase,
        code: &'static str,
    ) -> Result<CallResult, Error> {
        match phase {
            CallPhase::Open { .. } => self.deny(flow, code).await,
            CallPhase::Denied => Err(Error::Denied),
            _ => {
                let _ = self
                    .note(
                        &flow.attempt.profile,
                        &flow.ctx.principal,
                        "call.replay",
                        &flow.tool,
                        Some(flow.sealed.sha256),
                        code,
                    )
                    .await;
                Err(Error::Denied)
            }
        }
    }

    /// 즉시 활성화 직후 로컬 카탈로그와 저장소의 활성 판본이 방금 게시한 판본인지 확인한다.
    async fn verify_activated(&self, attempt: &Attempt, expected: &Revision) -> Result<(), Error> {
        let latest = self
            .parts
            .catalog
            .current(&attempt.profile)
            .ok_or(Error::Denied)?;
        if latest.revision != *expected {
            return Err(Error::Conflict);
        }
        match self.parts.ledger.active_revision(attempt.clone()).await? {
            Some(active) if active == *expected => Ok(()),
            Some(_) => Err(Error::Conflict),
            None => Err(Error::Denied),
        }
    }
}
