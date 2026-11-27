//! Approval gate and binding computation for tool, memory share, and scheduled delivery calls.

use super::{BrokerService, call::Flow, route::Route};
use crate::{
    approval::{new_nonce, render_card, render_delivery_card, render_memory_card, same_binding},
    classify::Class,
};
use bot_core::{
    api::ApprovalBlock,
    command::*,
    id::*,
    ledger::*,
    port::Error,
    profile::ApprovalRoute,
    schedule::{DELIVER_TOOL, UnsentReason},
    state::*,
};
use std::time::SystemTime;

pub(super) enum Gate {
    Go(Option<Box<(Approval, Vec<String>)>>),
    Return(CallResult),
}

/// Recomputed approval binding inputs shared between the call gate and approval decision paths.
pub(super) struct Binding<'a> {
    pub attempt: &'a Attempt,
    pub call: &'a CallId,
    pub requester: &'a PrincipalId,
    pub approver: &'a PrincipalId,
    pub class: &'a Class,
    pub policy_revision: &'a Revision,
    pub tool: &'a ToolId,
    pub tool_revision: &'a Revision,
    pub args_sha256: [u8; 32],
}

impl Binding<'_> {
    pub fn approval(&self) -> Approval {
        Approval {
            nonce: ApprovalId(String::new()),
            attempt: self.attempt.clone(),
            call: self.call.clone(),
            requester: self.requester.clone(),
            approver: self.approver.clone(),
            risk: self.class.risk,
            policy_revision: self.policy_revision.clone(),
            tool: self.tool.clone(),
            tool_revision: self.tool_revision.clone(),
            args_sha256: self.args_sha256,
            expires_at: SystemTime::UNIX_EPOCH,
        }
    }
}

impl BrokerService {
    pub(super) async fn gate(
        &self,
        flow: &Flow,
        class: &Class,
        pointer: Option<ApprovalId>,
        authority: Option<&bot_core::grant::Authority>,
    ) -> Result<Gate, Error> {
        // Direct-call authority waives only single-stage reasons; independent gates still open cards.
        if !class.needs_approval() || authority.is_some() && super::authority::waivable(class) {
            return Ok(Gate::Go(None));
        }
        let Some(approver) = flow.approver.as_ref() else {
            return match &flow.origin {
                Some(Err(why)) => self.deny_blocked(flow, *why).await,
                _ => self.deny(flow, "no_approver").await,
            };
        };
        let reasons = class.codes();
        let expected = Binding {
            attempt: &flow.attempt,
            call: &flow.call,
            requester: &flow.ctx.principal,
            approver,
            class,
            policy_revision: &flow.snapshot.policy_revision,
            tool: &flow.tool,
            tool_revision: &flow.resolved.tool.revision,
            args_sha256: flow.sealed.sha256,
        }
        .approval();
        let now = self.parts.clock.now();
        if let Some(nonce) = pointer {
            let stored = self.parts.ledger.approval(nonce.clone()).await?;
            let live = now < stored.approval.expires_at;
            let same = same_binding(&stored.approval, &expected) && stored.reasons == reasons;
            match stored.state {
                ApprovalState::Denied | ApprovalState::Revoked => {
                    if flow.tool.0 == DELIVER_TOOL
                        && let Some(target) = &flow.route
                    {
                        return self
                            .finish_unsent(&flow.attempt, target, UnsentReason::Rejected)
                            .await
                            .map(Gate::Return);
                    }
                    return self.deny(flow, "approval_denied").await;
                }
                ApprovalState::Expired => return self.deny(flow, "approval_expired").await,
                ApprovalState::Pending | ApprovalState::Approved if same && !live => {
                    return self.deny(flow, "approval_expired").await;
                }
                ApprovalState::Pending if same => {
                    return Ok(Gate::Return(CallResult::Awaiting(nonce)));
                }
                ApprovalState::Approved if same => {
                    // OpenAI-origin Slack cards must match the current installation `team_id` before consumption.
                    if let Some(Ok(found)) = &flow.origin {
                        match self.card_site(&stored, &flow.snapshot, found) {
                            Route::Same => {}
                            Route::Gone => {
                                return self.deny_blocked(flow, ApprovalBlock::RouteChanged).await;
                            }
                            Route::Unsure => return Err(Error::Unavailable),
                        }
                    }
                    let consumable = Approval {
                        nonce: stored.approval.nonce,
                        expires_at: stored.approval.expires_at,
                        ..expected
                    };
                    return Ok(Gate::Go(Some(Box::new((consumable, reasons)))));
                }
                // Consumed, superseded, or mismatched approvals cannot be reused; open a fresh card.
                _ => {}
            }
        }
        // Re-check explicit destination routes immediately before opening a card.
        if let Some(target) = &flow.route {
            match self.route_state(&flow.attempt.profile, target) {
                Route::Same => {}
                Route::Gone => return self.route_gone(flow, target).await.map(Gate::Return),
                Route::Unsure => return Err(Error::Unavailable),
            }
        }
        let nonce = new_nonce(self.parts.entropy.as_ref()).ok_or(Error::Unavailable)?;
        let card = match (&flow.memory, &flow.delivery) {
            (_, Some(document)) => {
                render_delivery_card(&nonce, class, &flow.ctx.principal, approver, document)
            }
            (Some((id, text)), None) => {
                render_memory_card(&nonce, class, &flow.ctx.principal, approver, id, text)
            }
            (None, None) => render_card(
                &nonce,
                &flow.tool,
                class,
                &flow.ctx.principal,
                approver,
                flow.card_args.as_deref().unwrap_or(&flow.sealed.canonical),
                flow.sealed.destination.as_ref(),
            ),
        };
        let card = match card {
            Ok(card) => card,
            Err(_) => return self.deny(flow, "card_too_large").await,
        };
        let approval = Approval {
            nonce: nonce.clone(),
            expires_at: now + self.cfg.approval_ttl,
            ..expected
        };
        self.parts
            .ledger
            .open_approval(OpenApproval {
                approval,
                reasons,
                card,
                // Attach delivery binding only for Slack DM approval routes (local routes use Admin UI).
                delivery: match &flow.origin {
                    Some(Ok(found)) => match &found.route {
                        ApprovalRoute::Slack { binding, .. } => Some(binding.clone()),
                        ApprovalRoute::Local { .. } => None,
                    },
                    _ => None,
                },
            })
            .await?;
        Ok(Gate::Return(CallResult::Awaiting(nonce)))
    }
}
