//! Builds ordered `bot/pass` fallback chains for direct tool triggers on authenticated Slack events.
use super::{Plan, plan_of};
use crate::{
    inbound::{Event, Inbound},
    routes::Route,
    trigger,
};
use bot_core::{
    command::Target,
    pass::{Candidate, PassChain},
    scope::RUN_SCOPE,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Collects matching direct-tool triggers in `order` up to the first instruction trigger (or private boundary).
/// Default agent fallback requires `RUN_SCOPE`; without it, only tool candidates remain.
pub(super) fn pass_chain(
    route: &Route<'_>,
    inbound: &Inbound,
    envelope: &Value,
    chosen: &Plan,
    scopes: &BTreeSet<String>,
) -> Option<PassChain> {
    if !matches!(chosen.target, Target::Tool { .. }) {
        return None;
    }
    let mut candidates = Vec::new();
    let mut default = None;
    for bound in trigger::select_all(route.triggers, route.binding_id, &inbound.event, envelope) {
        if bound.trigger.private {
            break;
        }
        let Some(plan) = plan_of(bound, envelope) else {
            continue;
        };
        match plan.target {
            Target::Tool { .. } => candidates.push(Candidate {
                trigger: bound.trigger.id.clone(),
                target: plan.target,
            }),
            target @ Target::Agent { .. } => {
                default = Some(target);
                break;
            }
        }
    }
    if candidates.first().map(|first| &first.target) != Some(&chosen.target) {
        return None;
    }
    if !scopes.contains(RUN_SCOPE) {
        default = None;
    } else if default.is_none() {
        default = default_input(inbound).map(|instruction| Target::Agent {
            agent: route.binding.agent.clone(),
            instruction,
        });
    }
    Some(PassChain {
        candidates,
        default,
    })
}

fn default_input(inbound: &Inbound) -> Option<String> {
    match inbound.event {
        Event::Reaction => reaction_event(&inbound.body),
        _ => inbound
            .body
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned),
    }
}

/// Fixed JSON payload for reaction fallback containing only validated `item_ts` and `reaction`.
fn reaction_event(body: &Value) -> Option<String> {
    let reaction = body.get("reaction")?.as_str()?;
    let item_ts = body.get("item_ts")?.as_str()?;
    let name_ok = !reaction.is_empty()
        && reaction.len() <= 100
        && reaction.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'+' | b'-' | b':')
        });
    (name_ok && crate::schedule_routes::is_ts(item_ts)).then(|| {
        json!({"event": "reaction_added", "item_ts": item_ts, "reaction": reaction}).to_string()
    })
}
