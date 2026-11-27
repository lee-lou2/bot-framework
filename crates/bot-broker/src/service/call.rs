//! Main `Broker::call` flow: declaration merge, secret admission, authorization, and call spans.
//!
//! Follows the call sequence in `service.rs`, delegating approval gating to `gate`,
//! execution/recovery to `exec`, and built-in tools to `share`, `schedule`, and `manage`.

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
};
use bot_core::{
    api::ApprovalBlock,
    command::*,
    id::*,
    ledger::*,
    port::Error,
    profile::{Snapshot, Tool},
    schedule::DeliveryTarget,
    scope::authorize,
    state::{Risk, RunContext},
    trace::{SpanIds, SpanKind, SpanRecord, key, now_nano},
};
use std::sync::Arc;

pub(super) struct Flow {
    pub attempt: Attempt,
    pub call: CallId,
    pub tool: ToolId,
    pub ctx: RunContext,
    /// Current active revision (policy, roles, and approver source).
    pub snapshot: Arc<Snapshot>,
    /// Revision pinned when the run started.
    pub pinned: Arc<Snapshot>,
    pub approver: Option<PrincipalId>,
    /// Approval route verdict for authenticated OpenAI-compatible HTTP runs (`None` for other origins).
    pub origin: Option<Result<openai::Origin, ApprovalBlock>>,
    pub resolved: Resolved,
    pub sealed: Sealed,
    pub env: Vec<(String, String)>,
    /// `(memory_id, body)` displayed on memory-share approval cards.
    pub memory: Option<(MemoryId, String)>,
    /// `DeliveryDraft::approval_json` document shown on scheduled-delivery approval cards.
    pub delivery: Option<String>,
    /// Canonical destination for cross-destination schedules, rechecked against the live registry before opening cards.
    pub route: Option<DeliveryTarget>,
    /// Redacted argument JSON for approval cards (e.g. `mcp.add`); falls back to `sealed.canonical` when `None`.
    pub card_args: Option<String>,
}

pub(super) struct Decl {
    pub resolved: Resolved,
    pub current: Arc<Snapshot>,
    pub pinned: Arc<Snapshot>,
    pub env: Vec<(String, String)>,
}

#[derive(Default)]
pub(super) struct Note {
    pub risk: Option<Risk>,
    pub revision: Option<String>,
}

/// Returns an audit code if any declared secret is host-reserved or outside `snapshot.secrets`.
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
        Error::Unavailable | Error::RateLimited => "unavailable",
    }
}

impl BrokerService {
    /// Merges pinned and active tool declarations and pins the installed server image into the tool revision.
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
        // Selected env values must match between pinned and active revisions.
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
        // Host-only delivery call ID can never be claimed via the general call path.
        if req.call.0 == bot_core::schedule::DELIVER_CALL {
            return self.refuse_reserved_call(&req.attempt, &req.tool).await;
        }
        if req.tool.0 == super::share::SHARE_TOOL {
            return self.share_flow(req, note).await;
        }
        if super::memory::is_builtin(&req.tool.0) {
            return self.memory_flow(req, note).await;
        }
        if super::schedule::is_builtin(&req.tool.0) {
            return self.schedule_flow(req, note).await;
        }
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
        // Pre-record secret check for new calls only: after sealing, forbidden-secret check, authorization,
        // and rate limiting so unauthorized/over-limit calls never touch SecretStore, while missing secrets
        // avoid opening approval cards. Unreadable stores are left to the runner; recorded replays proceed normally.
        let mut held = None;
        let bound = self.parts.runner.server_secrets(&resolved.tool.server);
        if (!resolved.tool.secrets.is_empty() || !bound.is_empty())
            && forbidden_secret(&resolved.tool, &snapshot).is_none()
            && ledger
                .lookup(attempt.clone(), call.clone())
                .await?
                .is_none()
            && authorize(&resolved.tool.scopes, &ctx, &snapshot, &pinned).is_ok()
        {
            // Keep this rate-limit slot for the remainder of the call so it is not double-counted.
            held = Some(
                self.limiter
                    .admit(&attempt.profile, &ctx.principal)
                    .map_err(|_| Error::RateLimited)?,
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
        if record.tool != tool || record.args_sha256 != sealed.sha256 {
            return Err(Error::Conflict);
        }
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
        // Re-authorize before replaying cached receipts or recovering so revoked roles cannot read prior results.
        if let Err(denial) = authorize(
            &flow.resolved.tool.scopes,
            &flow.ctx,
            &flow.snapshot,
            &flow.pinned,
        ) {
            return match &record.phase {
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
                self.still_current(&flow).await?;
                return Ok(CallResult::Done(result.clone()));
            }
            CallPhase::Denied => return Err(Error::Denied),
            CallPhase::Open { approval } => approval.clone(),
            CallPhase::Started { generation, .. } => {
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
        if let Some(code) = forbidden_secret(&flow.resolved.tool, &flow.snapshot) {
            return self.deny(&flow, code).await;
        }
        let _slot = match held {
            Some(slot) => slot,
            None => self
                .limiter
                .admit(&flow.attempt.profile, &flow.ctx.principal)
                .map_err(|_| Error::RateLimited)?,
        };
        let class = classify(
            &flow.resolved.tool,
            &flow.snapshot.policy.approve,
            flow.ctx.private_memory_read,
        );
        note.risk = Some(class.risk);
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
        // Card approvals cannot be combined with standing authority; pre-approved grants cannot fall back to cards.
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

    /// Audits and closes a call as blocked while persisting its closed `ApprovalBlock` reason.
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
