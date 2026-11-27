//! Approval routing for runs accepted through the OpenAI-compatible HTTP ingress.
//!
//! Resolves the approver from the root authenticated HTTP principal and original API binding in both
//! the run's pinned snapshot and the current active snapshot (`Snapshot::approval_route`). Any drift
//! or revocation blocks opening with a fixed `ApprovalBlock` code. Gated strictly on ledger-recorded
//! `RunContext.api`.

use bot_core::{
    api::ApprovalBlock,
    id::{AgentId, BindingId, PrincipalId},
    profile::{Adapter, ApprovalRoute, Snapshot},
    state::RunContext,
};

/// Origin metadata and approval route resolved for an OpenAI binding in one snapshot revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Origin {
    pub endpoint: String,
    pub model: String,
    pub agent: AgentId,
    pub route: ApprovalRoute,
}

impl Origin {
    pub fn in_snapshot(
        snapshot: &Snapshot,
        binding: &BindingId,
        requester: &PrincipalId,
    ) -> Option<Self> {
        let route = snapshot.approval_route(binding, requester)?;
        let found = snapshot.bindings.get(binding)?;
        let Adapter::OpenAi {
            endpoint, model, ..
        } = &found.source
        else {
            return None;
        };
        Some(Self {
            endpoint: endpoint.clone(),
            model: model.clone(),
            agent: found.agent.clone(),
            route,
        })
    }
}

/// Opens only when pinned and current revisions resolve to the identical origin and approval route.
pub fn open(pinned: Option<Origin>, current: Option<Origin>) -> Result<Origin, ApprovalBlock> {
    match (pinned, current) {
        (None, _) => Err(ApprovalBlock::NoApprover),
        (Some(_), None) => Err(ApprovalBlock::ApproverRevoked),
        (Some(pinned), Some(current)) if pinned == current => Ok(pinned),
        (Some(_), Some(_)) => Err(ApprovalBlock::RouteChanged),
    }
}

/// Resolves the OpenAI approval route if `ctx` originated from an OpenAI ingress (`RunContext.api`).
/// Returns `None` for non-OpenAI runs so callers fall back to the default requester-approver rule.
pub fn for_run(
    ctx: &RunContext,
    pinned: &Snapshot,
    current: &Snapshot,
) -> Option<Result<Origin, ApprovalBlock>> {
    let api = ctx.api.as_ref()?;
    let read = |snapshot: &Snapshot| Origin::in_snapshot(snapshot, &ctx.binding, &ctx.principal);
    let pinned = read(pinned).filter(|o| o.endpoint == api.endpoint && o.model == api.model);
    Some(open(pinned, read(current)))
}

#[cfg(test)]
mod tests;
