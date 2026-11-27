//! Direct-call authority evaluation (pre-approval grants and designated human approval evidence).
//!
//! Authority applies only to root direct `{run}:tool` calls and is never inherited by model calls
//! or delegated children; the store re-verifies the authority under the start lock.

use super::{BrokerService, call::Flow};
use crate::{
    classify::{Class, Reason},
    preapproval,
};
use bot_core::{
    command::Target,
    grant::{Authority, reason},
    profile::{Snapshot, Trigger, TriggerSource, TriggerTarget},
    state::ConversationKind,
};

pub(super) enum Verdict {
    /// No authority declaration applies; fall back to normal approval rules.
    None,
    Use(Authority),
    /// Authority is declared but unusable; close with this reason rather than falling back to cards.
    Deny(&'static str),
}

/// Returns true only when all approval reasons are single-stage (`Irreversible`/`Policy`),
/// preserving independent gates such as `PrivateRecall`.
pub(super) fn waivable(class: &Class) -> bool {
    class
        .reasons
        .iter()
        .all(|r| matches!(r, Reason::Irreversible | Reason::Policy))
}

fn owned<'a>(
    snapshot: &'a Snapshot,
    agent: &bot_core::id::AgentId,
    id: &str,
) -> Option<&'a Trigger> {
    snapshot
        .agents
        .get(agent)?
        .triggers
        .iter()
        .find(|t| t.id == id)
}

impl BrokerService {
    pub(super) async fn authority_for(&self, flow: &Flow, class: &Class, pointer: bool) -> Verdict {
        let ctx = &flow.ctx;
        let direct = ctx.root == flow.attempt.run
            && flow.call.0 == format!("{}:tool", flow.attempt.run.0)
            && matches!(&ctx.work.target, Target::Tool { tool, .. } if *tool == flow.tool);
        if !direct {
            return Verdict::None;
        }
        match ctx.conversation.kind {
            ConversationKind::Scheduled => self.grant_authority(flow),
            _ => self.human_authority(flow, class, pointer).await,
        }
    }

    fn grant_authority(&self, flow: &Flow) -> Verdict {
        let ctx = &flow.ctx;
        let Some(origin) = ctx.trigger.as_ref().filter(|o| o.agent == ctx.agent) else {
            return Verdict::None;
        };
        let Some(trigger) = owned(&flow.snapshot, &ctx.agent, &origin.id) else {
            return Verdict::None;
        };
        if trigger.preapproved.is_none() {
            return Verdict::None;
        }
        let same_principal = matches!(
            &trigger.on,
            TriggerSource::Cron { principal, .. } if *principal == ctx.principal
        );
        let image = self.parts.runner.installed(&flow.resolved.tool.server);
        let spec = image.filter(|_| same_principal).and_then(|image| {
            preapproval::grant_spec(
                &flow.snapshot,
                &ctx.agent,
                trigger,
                &ctx.scopes,
                &image,
                &flow.env,
            )
        });
        match spec {
            Some(spec) => Verdict::Use(Authority::Grant {
                fingerprint: spec.fingerprint,
            }),
            None => Verdict::Deny(reason::STALE),
        }
    }

    /// Uses intake human-approval evidence only when both current and pinned snapshots designate the
    /// same human approver on the matching event trigger; mismatches audit `HUMAN_MISMATCH` and fall back to cards.
    async fn human_authority(&self, flow: &Flow, class: &Class, pointer: bool) -> Verdict {
        if !class.needs_approval() || pointer {
            return Verdict::None;
        }
        let Ok(Some(evidence)) = self.parts.ledger.evidence(flow.attempt.clone()).await else {
            return Verdict::None;
        };
        let ctx = &flow.ctx;
        let designated = |snapshot: &Snapshot| {
            let Some(trigger) = owned(snapshot, &ctx.agent, &evidence.trigger) else {
                return false;
            };
            matches!(&trigger.on, TriggerSource::Event { binding, .. } if *binding == evidence.binding)
                && !trigger.private
                && matches!(&trigger.target, TriggerTarget::Tool { tool, .. } if *tool == flow.tool)
                && trigger
                    .human_approval
                    .as_ref()
                    .is_some_and(|h| h.approvers.contains(&evidence.actor))
                && evidence.actor.is_human()
                && snapshot.members.contains_key(&evidence.actor)
        };
        let fits = waivable(class)
            && ctx.principal == evidence.actor
            && ctx.binding == evidence.binding
            && designated(&flow.snapshot)
            && designated(&flow.pinned);
        if fits {
            return Verdict::Use(Authority::Human {
                actor: evidence.actor,
            });
        }
        let _ = self
            .note(
                &flow.attempt.profile,
                &ctx.principal,
                "call.note",
                &flow.tool,
                Some(flow.sealed.sha256),
                reason::HUMAN_MISMATCH,
            )
            .await;
        Verdict::None
    }
}
