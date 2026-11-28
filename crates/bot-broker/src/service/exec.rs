//! Post-commit tool execution, outcome settlement, and prior-generation call recovery.
//!
//! Invokes the runner only after `start` commits to the ledger, and re-verifies `still_current`
//! as the last `await` before returning any result to the caller.

use super::{BrokerService, call::Flow};
use crate::{
    classify::{Class, classify},
    ports::*,
    recovery::{Facts, Plan, plan},
    text::json_quote,
};
use bot_core::{
    command::*,
    id::Attempt,
    ledger::*,
    port::Error,
    profile::{Destination, Snapshot},
    state::*,
};

pub(super) enum Recovered {
    Finished(CallResult),
    /// Non-execution proved and call returned to `Open`; continue with current policy/approval checks.
    Open,
}

impl BrokerService {
    /// Cheap pre-start check against the local catalog (the ledger enforces `checked_revision` atomically under lock).
    pub(super) fn still_current_local(&self, flow: &Flow) -> Result<(), Error> {
        self.revision_local(&flow.attempt, &flow.snapshot)
    }

    fn revision_local(&self, attempt: &Attempt, snapshot: &Snapshot) -> Result<(), Error> {
        let latest = self
            .parts
            .catalog
            .current(&attempt.profile)
            .ok_or(Error::Denied)?;
        if latest.revision != snapshot.revision
            || latest.policy_revision != snapshot.policy_revision
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    /// Must remain the last `await` before returning results: verifies both the local catalog and the store's
    /// active revision and current claim so stale or superseded attempts never observe tool output.
    pub(super) async fn still_current(&self, flow: &Flow) -> Result<(), Error> {
        self.revision_current(&flow.attempt, &flow.snapshot).await
    }

    pub(super) async fn revision_current(
        &self,
        attempt: &Attempt,
        snapshot: &Snapshot,
    ) -> Result<(), Error> {
        self.revision_local(attempt, snapshot)?;
        match self.parts.ledger.active_revision(attempt.clone()).await? {
            Some(active) if active == snapshot.revision => Ok(()),
            Some(_) => Err(Error::Conflict),
            None => Err(Error::Denied),
        }
    }

    pub(super) async fn finish(&self, flow: &Flow, outcome: Outcome) -> Result<(), Error> {
        self.parts
            .ledger
            .finish(FinishCall {
                attempt: flow.attempt.clone(),
                call: flow.call.clone(),
                outcome,
            })
            .await
    }

    pub(super) async fn execute(
        &self,
        flow: &Flow,
        class: &Class,
        approval: Option<(Approval, Vec<String>)>,
        authority: Option<bot_core::grant::Authority>,
    ) -> Result<CallResult, Error> {
        let owner = format!(
            "{}.{}.{}",
            flow.attempt.run.0, flow.attempt.generation.0, flow.call.0
        );
        self.still_current_local(flow)?;
        let start = StartCall {
            checked_revision: flow.snapshot.revision.clone(),
            risk: class.risk,
            tool_revision: flow.resolved.tool.revision.clone(),
            attempt: flow.attempt.clone(),
            call: flow.call.clone(),
            approval,
            private_recall: flow.ctx.private_memory_read,
            owner: owner.clone(),
        };
        match authority {
            None => self.parts.ledger.start(start).await?,
            Some(authority) => match self.parts.ledger.start_with(start, authority).await? {
                bot_core::grant::Granted::Started => {}
                bot_core::grant::Granted::Refused(why) => return self.deny(flow, why).await,
            },
        }
        let reply_to = matches!(flow.resolved.tool.destination, Some(Destination::Reply))
            .then(|| flow.ctx.conversation.clone());
        let job = ToolJob {
            attempt: flow.attempt.clone(),
            principal: flow.ctx.principal.clone(),
            origin: flow.ctx.origin,
            call: flow.call.clone(),
            tool: flow.resolved.clone(),
            args: flow.sealed.canonical.clone(),
            env: flow.env.clone(),
            conversation: flow.ctx.conversation.clone(),
            reply_to,
            owner,
        };
        let ran = self
            .parts
            .runner
            .run(job)
            .await
            .unwrap_or(Ran::Unknown("runner_error"));
        match ran {
            Ran::Done(payload) => {
                if flow.resolved.tool.effects && !self.steps_settled(flow).await {
                    self.finish(flow, Outcome::Held("effects_unresolved".into()))
                        .await?;
                    return Ok(CallResult::Held(flow.call.clone()));
                }
                // Always strip `_meta["bot/pass"]`; honor it only on the current direct-trigger candidate.
                let (payload, marker) = super::pass::split(&payload);
                if let (Some(marker), Some(pos)) = (marker, Self::pass_position(flow)) {
                    return self
                        .finish_with_pass(flow, class, payload, marker, pos)
                        .await;
                }
                // Always strip `_meta["bot/wake"]`; commit any wake atomically with call completion.
                let extracted = super::wake::extract(&payload, self.parts.clock.now());
                if extracted.request.is_some() {
                    return self.finish_with_wake(flow, extracted).await;
                }
                self.finish(flow, Outcome::Done(payload.clone())).await?;
                self.still_current(flow).await?;
                Ok(CallResult::Done(payload))
            }
            // Server rejected before execution: no external side effects; model sees a fixed error payload.
            Ran::Rejected => {
                let payload = JsonPayload(crate::mcp::error_payload());
                self.finish(flow, Outcome::Done(payload.clone())).await?;
                self.still_current(flow).await?;
                Ok(CallResult::Done(payload))
            }
            // Broker refused before dispatch (e.g. tool definition mismatch): no side effects; close as denied.
            Ran::Refused(why) => {
                self.note(
                    &flow.attempt.profile,
                    &flow.ctx.principal,
                    "call.refuse",
                    &flow.tool,
                    Some(flow.sealed.sha256),
                    why,
                )
                .await?;
                self.finish(flow, Outcome::Blocked).await?;
                Err(Error::Denied)
            }
            Ran::NotSent => {
                self.finish(flow, Outcome::Released).await?;
                Err(Error::Unavailable)
            }
            // Reads are declared idempotent; writes with unknown delivery must be held.
            Ran::Unknown(_) if class.risk == Risk::Read => {
                self.finish(flow, Outcome::Released).await?;
                Err(Error::Unavailable)
            }
            Ran::Unknown(why) => {
                self.finish(flow, Outcome::Held(why.into())).await?;
                Ok(CallResult::Held(flow.call.clone()))
            }
            // Output was blocked after execution: deny reads (no external effect) but hold writes (unknown effect).
            Ran::Blocked(why) => {
                self.note(
                    &flow.attempt.profile,
                    &flow.ctx.principal,
                    "call.blocked",
                    &flow.tool,
                    Some(flow.sealed.sha256),
                    why,
                )
                .await?;
                if class.risk == Risk::Read {
                    self.finish(flow, Outcome::Blocked).await?;
                    return Err(Error::Denied);
                }
                self.finish(flow, Outcome::Held(why.into())).await?;
                Ok(CallResult::Held(flow.call.clone()))
            }
        }
    }

    async fn steps_settled(&self, flow: &Flow) -> bool {
        self.parts
            .effects
            .unresolved(flow.attempt.clone(), flow.call.clone())
            .await
            .is_ok_and(|steps| steps.is_empty())
    }

    /// Evaluates whether a started or held call from an earlier attempt can safely resume or settle.
    /// Admin resolutions on `Held` already assert prior termination and non-execution, skipping `reap`.
    pub(super) async fn recover(&self, flow: &Flow, phase: &CallPhase) -> Result<Recovered, Error> {
        let (reaped, resolution, started, confirmed_step) = match phase {
            // Closing a connection to a remote server does not prove remote execution stopped (`reap_proves_end`).
            CallPhase::Started {
                owner,
                confirmed_step,
                ..
            } => (
                self.parts.runner.reap(owner.clone()).await.is_ok()
                    && self
                        .parts
                        .runner
                        .reap_proves_end(&flow.resolved.tool.server),
                None,
                true,
                *confirmed_step,
            ),
            CallPhase::Held { resolution, .. } => (true, resolution.clone(), false, false),
            _ => return Ok(Recovered::Open),
        };
        let tool = &flow.resolved.tool;
        let unresolved = if tool.effects {
            self.parts
                .effects
                .unresolved(flow.attempt.clone(), flow.call.clone())
                .await
                .ok()
                .map(|steps| steps.len())
        } else {
            Some(0)
        };
        let risk = classify(tool, &flow.snapshot.policy.approve, false).risk;
        let facts = Facts {
            class: risk,
            effects: tool.effects,
            reaped,
            unresolved,
            confirmed_step,
            resolution,
        };
        match plan(&facts) {
            Plan::Resume => {
                self.finish(flow, Outcome::Released).await?;
                Ok(Recovered::Open)
            }
            Plan::Complete(receipt) => {
                let payload = JsonPayload(format!("{{\"receipt\":{}}}", json_quote(&receipt)));
                self.finish(flow, Outcome::Done(payload.clone())).await?;
                self.still_current(flow).await?;
                Ok(Recovered::Finished(CallResult::Done(payload)))
            }
            Plan::Hold(code) => {
                if started {
                    self.finish(flow, Outcome::Held(code.into())).await?;
                }
                Ok(Recovered::Finished(CallResult::Held(flow.call.clone())))
            }
        }
    }
}
