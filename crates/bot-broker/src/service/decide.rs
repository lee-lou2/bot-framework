//! Approver decision handling (`Broker::decide`) and live re-verification of approval card conditions.
//!
//! If policy, tool revision, approver, or destination route drifted after the card was opened, the
//! decision is rejected and the old approval is superseded (or settled as unrouted). Approval
//! consumption and tool execution happen later in `gate` and `exec` when the resumed run re-enters.

use super::{
    BrokerService,
    call::Decl,
    deliver::{deliver_approver, deliver_class, deliver_tool},
    gate::Binding,
    memory,
    route::{Route, created_destination, delivery_destination},
    schedule::{self, CREATE_TOOL, REVISION as SCHEDULE_REVISION, create_tool_to},
    share::{SHARE_REVISION, SHARE_TOOL, share_approver, share_class, share_tool},
};
use crate::{
    approval::same_binding,
    approver::{approver_for, openai},
    classify::classify,
};
use bot_core::{
    id::*,
    ledger::*,
    port::Error,
    profile::ApprovalRoute,
    schedule::{DELIVER_REVISION, DELIVER_TOOL, DeliveryFence, DeliveryTarget, UnsentReason},
    scope::authorize,
};

enum Check {
    Unchanged,
    Changed,
    /// Delivery card recipient was removed or re-pointed; close this occurrence as unsent.
    Unrouted(DeliveryTarget),
}

/// Verifies that a Slack-routed card has a confirmed delivery receipt matching the current approver, binding, and profile.
/// Rejects only the click (`Conflict` before posting, `Stale` on mismatch) without closing a valid card.
fn receipt_matches(stored: &StoredApproval, found: &openai::Origin) -> Result<(), Error> {
    let ApprovalRoute::Slack { person, binding } = &found.route else {
        return Ok(());
    };
    match &stored.delivery {
        Some(receipt)
            if receipt.owner == *person
                && receipt.binding == *binding
                && receipt.profile == stored.approval.attempt.profile =>
        {
            Ok(())
        }
        None => Err(Error::Conflict),
        Some(_) => Err(Error::Stale),
    }
}

impl BrokerService {
    /// Applies an approver's allow/deny decision after re-verifying that card conditions still hold.
    pub(super) async fn decide_flow(
        &self,
        nonce: ApprovalId,
        actor: PrincipalId,
        allow: bool,
    ) -> Result<(), Error> {
        let ledger = &self.parts.ledger;
        let stored = ledger.approval(nonce.clone()).await?;
        let approval = &stored.approval;
        // Only the designated human approver may decide; models and service principals never can.
        if actor != approval.approver || !actor.is_human() {
            let _ = self
                .note(
                    &approval.attempt.profile,
                    &actor,
                    "approval.decide",
                    &approval.tool,
                    Some(approval.args_sha256),
                    "actor_mismatch",
                )
                .await;
            return Err(Error::Denied);
        }
        match (stored.state, allow) {
            (ApprovalState::Approved | ApprovalState::Consumed, true)
            | (ApprovalState::Denied | ApprovalState::Revoked, false) => return Ok(()),
            (ApprovalState::Consumed, false) | (ApprovalState::Denied, true) => {
                return Err(Error::Conflict);
            }
            (ApprovalState::Expired | ApprovalState::Superseded | ApprovalState::Revoked, _) => {
                return Err(Error::Stale);
            }
            // Approved decisions may be revoked before consumption.
            (ApprovalState::Approved, false) => {
                return ledger
                    .decide(Decision {
                        nonce,
                        actor,
                        allow,
                    })
                    .await
                    .map(|_| ());
            }
            (ApprovalState::Pending, _) => {}
        }
        if self.parts.clock.now() >= approval.expires_at {
            return Err(Error::Stale);
        }
        let ctx = ledger
            .approval_context(nonce.clone(), actor.clone())
            .await?;
        if allow {
            match self.conditions_unchanged(&stored, &ctx).await {
                Ok(Check::Unchanged) => {}
                Ok(Check::Changed) | Err(Error::Denied | Error::Missing | Error::Invalid) => {
                    ledger.supersede(nonce, actor).await?;
                    return Err(Error::Stale);
                }
                // Close unrouted scheduled delivery atomically (supersede card + notice + unsent receipt) using the approval fence.
                Ok(Check::Unrouted(target)) => {
                    self.replies
                        .finish_unsent(
                            DeliveryFence::Approval {
                                nonce: nonce.clone(),
                                actor: actor.clone(),
                            },
                            target,
                            UnsentReason::Unrouted,
                        )
                        .await?;
                    return Err(Error::Stale);
                }
                Err(error) => return Err(error),
            }
        }
        ledger
            .decide(Decision {
                nonce,
                actor,
                allow,
            })
            .await
            .map(|_| ())
    }

    async fn conditions_unchanged(
        &self,
        stored: &StoredApproval,
        ctx: &bot_core::state::RunContext,
    ) -> Result<Check, Error> {
        let approval = &stored.approval;
        let (snapshot, tool_revision, class, approver) = if approval.tool.0 == SHARE_TOOL {
            let catalog = &self.parts.catalog;
            let profile = &approval.attempt.profile;
            let current = catalog.current(profile).ok_or(Error::Denied)?;
            let pinned = catalog
                .lookup(profile, &ctx.work.revision)
                .ok_or(Error::Missing)?;
            authorize(&share_tool().scopes, ctx, &current, &pinned).map_err(|_| Error::Denied)?;
            let approver = share_approver(ctx, &current);
            (
                current,
                Revision(SHARE_REVISION.into()),
                share_class(),
                approver,
            )
        } else if let Some(spec) = memory::mutating_tool(&approval.tool.0) {
            let catalog = &self.parts.catalog;
            let profile = &approval.attempt.profile;
            let current = catalog.current(profile).ok_or(Error::Denied)?;
            let pinned = catalog
                .lookup(profile, &ctx.work.revision)
                .ok_or(Error::Missing)?;
            authorize(&spec.scopes, ctx, &current, &pinned).map_err(|_| Error::Denied)?;
            let approver = share_approver(ctx, &current);
            let class = classify(&spec, &current.policy.approve, ctx.private_memory_read);
            (
                current,
                Revision(bot_core::builtin::REVISION.into()),
                class,
                approver,
            )
        } else if approval.tool.0 == DELIVER_TOOL {
            let catalog = &self.parts.catalog;
            let profile = &approval.attempt.profile;
            let current = catalog.current(profile).ok_or(Error::Denied)?;
            let pinned = catalog
                .lookup(profile, &ctx.work.revision)
                .ok_or(Error::Missing)?;
            authorize(&deliver_tool().scopes, ctx, &current, &pinned).map_err(|_| Error::Denied)?;
            if !ctx.principal.is_human() {
                return Ok(Check::Changed);
            }
            let target = delivery_destination(&stored.card, approval.args_sha256)?;
            match self.route_state(profile, &target) {
                Route::Same => {}
                Route::Gone => return Ok(Check::Unrouted(target)),
                // Unconfirmed registry leaves the button intact so the click can be retried.
                Route::Unsure => return Err(Error::Unavailable),
            }
            let class = deliver_class(&current, ctx);
            let approver = deliver_approver(ctx, &current);
            (current, Revision(DELIVER_REVISION.into()), class, approver)
        } else if let Some(spec) = schedule::mutating_tool(&approval.tool.0) {
            let catalog = &self.parts.catalog;
            let profile = &approval.attempt.profile;
            let current = catalog.current(profile).ok_or(Error::Denied)?;
            let pinned = catalog
                .lookup(profile, &ctx.work.revision)
                .ok_or(Error::Missing)?;
            authorize(&spec.scopes, ctx, &current, &pinned).map_err(|_| Error::Denied)?;
            if !ctx.principal.is_human() {
                return Ok(Check::Changed);
            }
            let destination = if approval.tool.0 == CREATE_TOOL {
                created_destination(&stored.card, approval.args_sha256)?
            } else {
                None
            };
            let spec = match &destination {
                Some(target) => match self.route_state(profile, target) {
                    Route::Same => create_tool_to(),
                    Route::Gone => return Ok(Check::Changed),
                    Route::Unsure => return Err(Error::Unavailable),
                },
                None => spec,
            };
            let class = classify(&spec, &current.policy.approve, ctx.private_memory_read);
            let approver = approver_for(ctx, &current);
            (current, Revision(SCHEDULE_REVISION.into()), class, approver)
        } else if let Some(spec) = bot_core::builtin::manage_tool(&approval.tool.0) {
            let catalog = &self.parts.catalog;
            let profile = &approval.attempt.profile;
            let current = catalog.current(profile).ok_or(Error::Denied)?;
            let pinned = catalog
                .lookup(profile, &ctx.work.revision)
                .ok_or(Error::Missing)?;
            if !current.is_super_admin(&ctx.principal) || !pinned.is_super_admin(&ctx.principal) {
                return Err(Error::Denied);
            }
            let decl = spec.declaration();
            authorize(&decl.scopes, ctx, &current, &pinned).map_err(|_| Error::Denied)?;
            let class = classify(&decl, &current.policy.approve, ctx.private_memory_read);
            let approver = match openai::for_run(ctx, &pinned, &current) {
                Some(Ok(found)) => {
                    receipt_matches(stored, &found)?;
                    match self.card_site(stored, &current, &found) {
                        Route::Same => {}
                        Route::Gone => return Ok(Check::Changed),
                        Route::Unsure => return Err(Error::Unavailable),
                    }
                    Some(found.route.person().clone())
                }
                Some(Err(_)) => None,
                None => approver_for(ctx, &current),
            };
            (
                current,
                Revision(bot_core::builtin::REVISION.into()),
                class,
                approver,
            )
        } else {
            let Decl {
                resolved,
                current: snapshot,
                pinned,
                ..
            } = self.declarations(ctx, &approval.attempt.profile, &approval.tool)?;
            let class = classify(
                &resolved.tool,
                &snapshot.policy.approve,
                ctx.private_memory_read,
            );
            let approver = match openai::for_run(ctx, &pinned, &snapshot) {
                Some(Ok(found)) => {
                    receipt_matches(stored, &found)?;
                    // Verify the Slack installation (`team_id`) still matches so redeployed aliases cannot reuse old cards.
                    match self.card_site(stored, &snapshot, &found) {
                        Route::Same => {}
                        Route::Gone => return Ok(Check::Changed),
                        Route::Unsure => return Err(Error::Unavailable),
                    }
                    Some(found.route.person().clone())
                }
                Some(Err(_)) => None,
                None => approver_for(ctx, &snapshot),
            };
            (snapshot, resolved.tool.revision.clone(), class, approver)
        };
        let Some(approver) = approver else {
            return Ok(Check::Changed);
        };
        let approver = &approver;
        let expected = Binding {
            attempt: &approval.attempt,
            call: &approval.call,
            requester: &ctx.principal,
            approver,
            class: &class,
            policy_revision: &snapshot.policy_revision,
            tool: &approval.tool,
            tool_revision: &tool_revision,
            args_sha256: approval.args_sha256,
        }
        .approval();
        Ok(
            if same_binding(approval, &expected) && stored.reasons == class.codes() {
                Check::Unchanged
            } else {
                Check::Changed
            },
        )
    }
}
