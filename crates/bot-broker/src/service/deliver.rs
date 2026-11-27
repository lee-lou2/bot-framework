//! Host-only release path (`schedule.deliver`, `ScheduleDelivery::release`) for scheduled replies to another recipient.
//!
//! Invisible to models (`call.rs` reserves `schedule-deliver`). Checks an existing receipt first
//! because the final commit closes the run and makes normal ledger calls `Stale`. Freezes a single
//! draft read to hash and render the full approval card; rejects without recording if the card
//! exceeds line limits. `ScheduledReplies::enqueue` consumes the approval, enqueues delivery, and
//! completes the run in one transaction; any draft or route drift closes with `Conflict` and 0 sends.

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

/// Built-in declaration requiring `schedule.create` so delivery verifies the creator still holds it.
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
        // Outbound to another recipient: triggers `private_recall` approval reason if private memory was read.
        external: true,
        destination: None,
        definition: None,
        mcp_name: None,
        timeout_secs: None,
    }
}

pub(super) fn deliver_class(current: &Snapshot, ctx: &RunContext) -> Class {
    classify(
        &deliver_tool(),
        &current.policy.approve,
        ctx.private_memory_read,
    )
}

/// Approver must be the human requester who created the schedule and remain a current member.
pub(super) fn deliver_approver(ctx: &RunContext, current: &Snapshot) -> Option<PrincipalId> {
    approver_for(ctx, current).filter(|approver| *approver == ctx.principal && approver.is_human())
}

enum Drafted {
    Draft(DeliveryDraft),
    Finished(CallResult),
}

/// Stub `ScheduledReplies` used when no reply store is wired.
pub(super) struct NoReplies;

impl ScheduledReplies for NoReplies {
    fn receipt(&self, _: Attempt) -> Reply<'_, Option<JsonPayload>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn draft(
        &self,
        _: Attempt,
        _: Option<bot_core::command::Finish>,
    ) -> Reply<'_, Option<bot_core::schedule::DeliveryDraft>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn target(&self, _: Attempt) -> Reply<'_, Option<bot_core::schedule::DeliveryTarget>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn finish_unsent(
        &self,
        _: bot_core::schedule::DeliveryFence,
        _: bot_core::schedule::DeliveryTarget,
        _: bot_core::schedule::UnsentReason,
    ) -> Reply<'_, JsonPayload> {
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
        // Check receipt first: a committed delivery closes the run, making normal ledger calls `Stale`.
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
        let Some(approver) = deliver_approver(&ctx, &current) else {
            return self
                .refuse_schedule(&attempt, &ctx, &tool, "not_owner")
                .await;
        };
        let draft = match self.draft_or_unrouted(&attempt).await? {
            Drafted::Draft(draft) => draft,
            Drafted::Finished(result) => return Ok(result),
        };
        // Verify the route still matches the host registry; drift finishes unsent unless owner-rejected.
        match self.route_state(&attempt.profile, &draft.target) {
            Route::Same => {}
            route => return self.unsettled_route(&attempt, &draft.target, route).await,
        }
        // Derive card document and approval hash from the same bytes.
        let Ok(document) = draft.approval_json() else {
            return self
                .refuse_schedule(&attempt, &ctx, &tool, "card_unsupported")
                .await;
        };
        let hash = sha256(&document);
        let class = deliver_class(&current, &ctx);
        // Refuse before recording if the full document cannot fit within card line limits.
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
        // Reject if the stored call disagrees on tool or draft hash.
        if record.tool != tool || record.args_sha256 != hash {
            return Err(Error::Conflict);
        }
        let flow = Flow {
            // Owner-only path: never uses the OpenAI delegated-approver route.
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
        // Defense in depth: irreversible delivery never proceeds without approval.
        let Some(approval) = approval else {
            return self.deny(&flow, "approval_missing").await;
        };
        self.still_current_local(&flow)?;
        // Re-check route right before consuming approval; drift finishes unsent without consuming it.
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
        // Atomically consume approval, enqueue destination outbox, and finish the run.
        if let Err(error) = self.replies.enqueue(start).await {
            // If the owner revoked concurrently, close as `Rejected` instead of surfacing raw `Denied`/`Conflict`.
            if matches!(error, Error::Denied | Error::Conflict)
                && self.owner_rejected(&flow.attempt).await?
            {
                return self
                    .finish_unsent(&flow.attempt, &draft.target, UnsentReason::Rejected)
                    .await;
            }
            return Err(error);
        }
        // Run is now closed; read the receipt directly via the dedicated store path.
        let receipt = self
            .replies
            .receipt(flow.attempt.clone())
            .await?
            .ok_or(Error::Unavailable)?;
        Ok(CallResult::Done(receipt))
    }

    /// Reads the frozen draft; on active-set `Conflict`, checks owner rejection and route drift via
    /// `target` first so an unrouted or rejected occurrence still closes cleanly.
    async fn draft_or_unrouted(&self, attempt: &Attempt) -> Result<Drafted, Error> {
        match self.replies.draft(attempt.clone(), None).await {
            Ok(Some(draft)) => Ok(Drafted::Draft(draft)),
            Ok(None) => Err(Error::Missing),
            Err(Error::Conflict) => {
                let Some(target) = self.replies.target(attempt.clone()).await? else {
                    return Err(Error::Conflict);
                };
                // Owner rejection takes precedence over active-set conflict or missing route.
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

    /// Denies attempts on the normal call path to claim host-reserved call IDs without recording.
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
