//! Built-in `memory.share` flow binding the exact previewed memory body to owner approval.
//!
//! Reads a single memory via `MemoryStore::preview` (no scan), derives both the full-body approval card
//! and `memory_share_hash` from that read, and commits approval consumption, visibility change, and call
//! completion atomically via `MemoryStore::share` (returning `Conflict` without consuming approval if the body changed).

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
    text::json_quote,
};
use bot_core::{
    command::{CallResult, JsonPayload, ToolCall, memory_share_hash},
    id::*,
    ledger::{CallPhase, StartCall},
    port::Error,
    profile::{Snapshot, Tool},
    scope::authorize,
    state::{ConversationKind, Risk, RunContext, Visibility},
};
use serde_json::Value;

pub use bot_core::builtin::{REVISION as SHARE_REVISION, SHARE_TOOL};

pub fn share_tool() -> Tool {
    bot_core::builtin::SHARE.declaration()
}

pub fn share_class() -> Class {
    Class {
        risk: Risk::Irreversible,
        reasons: vec![Reason::Irreversible],
        external: false,
    }
}

/// Only the human owner in their own 1:1 conversation (DM or CLI) may approve `memory.share`.
pub(super) fn share_approver(ctx: &RunContext, current: &Snapshot) -> Option<PrincipalId> {
    matches!(
        ctx.conversation.kind,
        ConversationKind::Direct | ConversationKind::Cli
    )
    .then(|| approver_for(ctx, current))
    .flatten()
    .filter(|a| *a == ctx.principal && a.is_human())
}

pub(super) fn judged(args: &JsonPayload) -> Option<bot_core::cli::CliRefusal> {
    memory_id(args)
        .is_err()
        .then_some(bot_core::cli::CliRefusal::Invalid)
}

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

    /// Replays a completed `memory.share` receipt only when the recorded tool and `{"id"}` match exactly
    /// and `revision_current` succeeds as the final `await`.
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
        if let Some(receipt) = self.replayed(&attempt, &call, &id, &current).await? {
            return Ok(CallResult::Done(receipt));
        }
        let preview = match memory.preview(attempt.clone(), id.clone()).await {
            Ok(preview) => preview,
            Err(error) => {
                // Race: a concurrent redelivery may have already completed (and the memory may since have been edited/deleted).
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
        // Re-read context because `preview` may have set the request tree's private-memory flag.
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
                if let Some(receipt) = self.replayed(&attempt, &call, &id, &current).await? {
                    return Ok(CallResult::Done(receipt));
                }
                return Err(error);
            }
        };
        if matches!(record.phase, CallPhase::Open { .. })
            && preview.visibility != Visibility::Private
        {
            return Err(Error::Conflict);
        }
        let flow = Flow {
            // Owner-only feature: never routed through HTTP/OpenAI approval bindings.
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
            card_args: None,
        };
        let pointer = match &record.phase {
            CallPhase::Done(_) => {
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
            .map_err(|_| Error::RateLimited)?;
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
        let shared = memory.share(start, id).await?;
        let result = JsonPayload(format!("{{\"id\":{}}}", json_quote(&shared.0)));
        self.still_current(&flow).await?;
        Ok(CallResult::Done(result))
    }
}

fn receipt_id(result: &JsonPayload) -> Option<String> {
    let Value::Object(map) = parse(&result.0)? else {
        return None;
    };
    match (map.len(), map.get("id")) {
        (1, Some(Value::String(id))) => Some(id.clone()),
        _ => None,
    }
}
