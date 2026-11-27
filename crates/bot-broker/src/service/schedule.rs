//! Built-in `schedule.create`, `schedule.list`, and `schedule.cancel` broker handlers.
//!
//! Only authenticated human principals may manage schedules, and explicit destinations must match
//! the profile's host route registry and receive owner approval. Mutating calls commit approval
//! consumption, schedule mutation, and call completion atomically through `UserSchedules`;
//! `schedule.deliver` is host-only and hidden from model callers (`Missing`).

use super::{
    BrokerService,
    call::{Flow, Note},
    gate::Gate,
    route::Route,
};
use crate::{approver::approver_for, classify::classify, resolve::Resolved};
use bot_core::{
    command::{CallResult, JsonPayload, ToolCall},
    id::*,
    ledger::{CallPhase, Outcome, StartCall},
    port::Error,
    profile::{Scopes, Snapshot, Tool},
    schedule::{DeliveryTarget, ScheduleSpec},
    scope::authorize,
    state::{ConversationKind, Risk, RunContext},
};

mod args;
use args::{Create, Reject, legacy_create_sha256, parse_cancel, parse_list, render_list};

pub use bot_core::builtin::{CANCEL_TOOL, CREATE_TOOL, DELIVER_TOOL, LIST_TOOL, REVISION};

/// Checks whether built-in schedule arguments are deterministically invalid before execution
/// (including `at` timestamps already in the past, which stay invalid as time advances).
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

pub fn is_builtin(tool: &str) -> bool {
    matches!(tool, CREATE_TOOL | LIST_TOOL | CANCEL_TOOL | DELIVER_TOOL)
}

pub fn create_tool() -> Tool {
    bot_core::builtin::CREATE.declaration()
}

pub fn list_tool() -> Tool {
    bot_core::builtin::LIST.declaration()
}

/// Explicit-destination creation is always irreversible and external, requiring owner approval
/// (and adding `private_recall` if private memory was read).
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

pub fn mutating_tool(id: &str) -> Option<Tool> {
    match id {
        CREATE_TOOL => Some(create_tool()),
        CANCEL_TOOL => Some(cancel_tool()),
        _ => None,
    }
}

pub(super) struct Gatekeep {
    pub(super) ctx: RunContext,
    pub(super) current: std::sync::Arc<Snapshot>,
    pub(super) pinned: std::sync::Arc<Snapshot>,
}

enum Plan {
    Create(Box<Create>),
    Cancel(ScheduleId),
    List,
}

enum Work {
    Create(ScheduleSpec),
    Cancel(ScheduleId),
    List,
}

impl BrokerService {
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
            _ => Err(Error::Missing),
        }
    }

    /// Replays a completed receipt after re-verifying claim and active revision.
    /// `legacy` matches old-format create fingerprints only for already-`Done` records, never open/started calls.
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

    /// Reads the store-recorded completion receipt so initial responses and retries stay identical.
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
        // Fill default timezone from the pinned snapshot and resolve explicit destinations before sealing
        // so card display, approval hash, and persisted spec agree.
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
        // Check completed replay before time-dependent validation so late redeliveries still return their receipt.
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
        let route = match &work {
            Work::Create(spec) => spec.destination.clone(),
            _ => None,
        };
        let flow = Flow {
            // Owner-only built-in: never routes through OpenAI delegated approval paths.
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
            card_args: None,
        };
        let pointer = match &record.phase {
            CallPhase::Done(result) => {
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
            .map_err(|_| Error::RateLimited)?;
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
        // Re-check explicit destination right before consuming approval so a removed route neither consumes nor creates.
        if let Some(target) = &flow.route {
            match self.route_state(&flow.attempt.profile, target) {
                Route::Same => {}
                Route::Gone => return self.route_gone(&flow, target).await,
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
            Work::Create(spec) => {
                self.schedules.create(start, spec).await?;
                self.recorded_receipt(&flow).await?
            }
            Work::Cancel(id) => {
                self.schedules.cancel(start, id).await?;
                self.recorded_receipt(&flow).await?
            }
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
        self.still_current(&flow).await?;
        Ok(CallResult::Done(result))
    }
}

mod unavailable;
pub(super) use unavailable::NoSchedules;

#[cfg(test)]
mod tests;
