//! Manual JSON serializers mapping `bot_core::admin` views to fixed admin API response shapes.

use bot_core::{
    admin::*,
    builtin::{CardBody, card_body},
    cards::{LABEL_BODY, LABEL_MEMORY_ID},
    command::memory_share_hash,
    id::MemoryId,
    schedule::DELIVERY_CARD_LABEL,
    state::{ConversationKind, EffectState, Risk, RunState},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::SystemTime;

pub fn hex(bytes: &[u8]) -> String {
    bot_core::random::hex(bytes)
}

pub fn preapproval(row: &bot_core::grant::PreapprovalRow) -> Value {
    json!({
        "profile": row.profile,
        "agent": row.agent,
        "trigger": row.trigger,
        "grant_id": row.grant_id,
        "epoch": row.epoch,
        "paused": row.paused,
        "used_today": row.used_today,
        "max_per_day": row.max_per_day,
    })
}

pub fn ms(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

pub fn run_state(s: RunState) -> &'static str {
    match s {
        RunState::Ready => "ready",
        RunState::Leased => "leased",
        RunState::Waiting => "waiting",
        RunState::Retry => "retry",
        RunState::Succeeded => "succeeded",
        RunState::Dead => "dead",
        RunState::Cancelled => "cancelled",
    }
}

pub fn parse_run_state(s: &str) -> Option<RunState> {
    [
        RunState::Ready,
        RunState::Leased,
        RunState::Waiting,
        RunState::Retry,
        RunState::Succeeded,
        RunState::Dead,
        RunState::Cancelled,
    ]
    .into_iter()
    .find(|x| run_state(*x) == s)
}

pub fn risk(r: Risk) -> &'static str {
    match r {
        Risk::Read => "read",
        Risk::Write => "write",
        Risk::Irreversible => "irreversible",
    }
}

fn conversation(k: &ConversationKind) -> &'static str {
    match k {
        ConversationKind::Direct => "direct",
        ConversationKind::Channel => "channel",
        ConversationKind::Cli => "cli",
        ConversationKind::Http => "http",
        ConversationKind::Scheduled => "scheduled",
    }
}

fn effect_state(s: EffectState) -> &'static str {
    match s {
        EffectState::Started => "started",
        EffectState::Confirmed => "confirmed",
        EffectState::Rejected => "rejected",
        EffectState::Held => "held",
    }
}

fn call_state(s: CallState) -> &'static str {
    match s {
        CallState::Running => "running",
        CallState::Awaiting => "awaiting",
        CallState::Done => "done",
        CallState::Held => "held",
        CallState::Denied => "denied",
        CallState::Failed => "failed",
    }
}

pub fn approval_state(s: ApprovalState) -> &'static str {
    match s {
        ApprovalState::Pending => "pending",
        ApprovalState::Approved => "approved",
        ApprovalState::Consumed => "consumed",
        ApprovalState::Denied => "denied",
        ApprovalState::Expired => "expired",
        ApprovalState::Superseded => "superseded",
        ApprovalState::Revoked => "revoked",
    }
}

pub fn parse_approval_filter(s: &str) -> Option<ApprovalFilter> {
    match s {
        "pending" => Some(ApprovalFilter::Pending),
        "decided" => Some(ApprovalFilter::Decided),
        "all" => Some(ApprovalFilter::All),
        _ => None,
    }
}

fn waiting(w: &Option<Waiting>) -> Value {
    match w {
        None => Value::Null,
        Some(Waiting::Approval) => json!({"on": "approval"}),
        Some(Waiting::Child(r)) => json!({"on": "child", "run": r.0}),
        Some(Waiting::Hold(c)) => json!({"on": "hold", "call": c.0}),
    }
}

fn trace_id(r: &RunView) -> String {
    let at = bot_core::id::Attempt {
        profile: r.profile.clone(),
        run: r.run.clone(),
        claim: None,
        generation: r.generation,
    };
    hex(&crate::trace::SpanIds::attempt(&at).trace)
}

pub fn run(r: &RunView) -> Value {
    json!({
        "trace_id": trace_id(r),
        "profile": r.profile.0, "run": r.run.0, "root": r.root.0, "parent": r.parent.as_ref().map(|p| &p.0),
        "step": r.step, "agent": r.agent.0, "principal": r.principal.0, "kind": conversation(&r.kind),
        "trigger": r.trigger, "state": run_state(r.state), "generation": r.generation.0, "attempts": r.attempts,
        "waiting": waiting(&r.waiting), "private_memory_read": r.private_memory_read, "error": r.error,
        "created_ms": ms(r.created_at), "updated_ms": ms(r.updated_at),
    })
}

pub fn call(c: &CallView) -> Value {
    json!({
        "call": c.call.0, "run": c.run.0, "generation": c.generation.0, "tool": c.tool.0,
        "tool_revision": c.tool_revision.0, "risk": risk(c.risk), "state": call_state(c.state),
        "error": c.error.map(CallError::as_str),
        "args_sha256": hex(&c.args_sha256), "started_ms": ms(c.started_at), "finished_ms": c.finished_at.map(ms),
    })
}

pub fn effect(e: &EffectView) -> Value {
    json!({
        "effect": e.effect.0, "run": e.run.0, "call": e.call.0, "step": e.step, "state": effect_state(e.state),
        "hold_id": e.hold_id.as_ref().map(|h| &h.0),
        "resolution": e.resolution.map(|r| match r { EffectResolution::NotExecuted => "not_executed" }),
        "receipt": e.receipt, "reason": e.reason, "args_sha256": hex(&e.args_sha256),
        "generation": e.generation.0, "at_ms": ms(e.at),
    })
}

pub fn detail(d: &RunDetail) -> Value {
    json!({
        "run": run(&d.run),
        "control": d.control.as_ref().map(|control| json!({
            "claim_epoch": control.claim_epoch.0,
            "cancel_requested": control.cancel_requested,
            "held": control.held,
        })),
        "lineage": d.lineage.iter().map(run).collect::<Vec<_>>(),
        "children": d.children.iter().map(run).collect::<Vec<_>>(),
        "calls": d.calls.iter().map(call).collect::<Vec<_>>(),
        "effects": d.effects.iter().map(effect).collect::<Vec<_>>(),
    })
}

/// Maps store-reported `HoldKind` directly without inspecting tool names or call IDs.
pub fn hold_kind(h: &HoldView) -> &'static str {
    match h.kind {
        HoldKind::Tool => "tool",
        HoldKind::Reply => "reply",
        HoldKind::Private => "private",
    }
}

pub fn hold(h: &HoldView) -> Value {
    json!({
        "profile": h.profile.0, "run": h.run.0, "call": h.call.0, "tool": h.tool.0, "reason": h.reason,
        "kind": hold_kind(h), "hold_id": h.hold_id.as_ref().map(|x| &x.0),
        "since_ms": ms(h.since), "steps": h.steps.iter().map(effect).collect::<Vec<_>>(),
    })
}

/// Owner-only card bodies are visible only when the viewer is both requester and approver (operators included).
pub fn body_visible(a: &ApprovalView, viewer: &Viewer) -> bool {
    card_body(&a.tool.0) == CardBody::None
        || (viewer.principal == a.requester && viewer.principal == a.approver)
}

/// Verifies that a memory-share card renders a single `LABEL_MEMORY_ID` and `LABEL_BODY` matching `args_sha256`.
fn share_card_matches(a: &ApprovalView) -> bool {
    let only = |label: &str| {
        let mut rows = a.render.iter().filter(|(k, _)| k == label);
        match (rows.next(), rows.next()) {
            (Some((_, v)), None) => Some(v.as_str()),
            _ => None,
        }
    };
    let (Some(id), Some(body)) = (only(LABEL_MEMORY_ID), only(LABEL_BODY)) else {
        return false;
    };
    let id = MemoryId(id.to_owned());
    let hash_of = |text: &str| memory_share_hash(&id, &Sha256::digest(text.as_bytes()).into());
    hash_of(body) == a.args_sha256
        || serde_json::from_str::<String>(body).is_ok_and(|text| hash_of(&text) == a.args_sha256)
}

/// Verifies that a scheduled-delivery card renders a single `DELIVERY_CARD_LABEL` row matching `args_sha256`.
fn delivery_card_matches(a: &ApprovalView) -> bool {
    let mut rows = a.render.iter().filter(|(k, _)| k == DELIVERY_CARD_LABEL);
    match (rows.next(), rows.next()) {
        (Some((_, draft)), None) => {
            <[u8; 32]>::from(Sha256::digest(draft.as_bytes())) == a.args_sha256
        }
        _ => false,
    }
}

/// Blocks positive approval (`"no_body"`) when an owner-only card body is hidden or fails digest verification.
pub fn approve_blocked(a: &ApprovalView, viewer: &Viewer) -> Option<&'static str> {
    let complete = match card_body(&a.tool.0) {
        CardBody::None => return None,
        CardBody::Share => share_card_matches(a),
        CardBody::Draft => delivery_card_matches(a),
    };
    (!body_visible(a, viewer) || !complete).then_some("no_body")
}

pub fn approval(a: &ApprovalView, viewer: &Viewer) -> Value {
    let visible = body_visible(a, viewer);
    let render: Vec<Value> = if visible {
        a.render
            .iter()
            .map(|(k, v)| json!({"label": k, "value": v}))
            .collect()
    } else {
        Vec::new()
    };
    json!({
        "nonce": a.nonce.0, "profile": a.profile.0, "run": a.run.0, "generation": a.generation.0, "call": a.call.0,
        "requester": a.requester.0, "approver": a.approver.0, "risk": risk(a.risk), "tool": a.tool.0,
        "tool_revision": a.tool_revision.0, "policy_revision": a.policy_revision.0,
        "args_sha256": hex(&a.args_sha256), "expires_ms": ms(a.expires_at), "state": approval_state(a.state),
        "render": render, "render_hidden": !visible, "approve_blocked": approve_blocked(a, viewer),
        "decided_by": a.decided_by.as_ref().map(|p| &p.0), "decided_ms": a.decided_at.map(ms),
    })
}

pub fn audit(e: &AuditEntry) -> Value {
    json!({
        "seq": e.seq, "at_ms": ms(e.at), "prev": hex(&e.prev), "hash": hex(&e.hash),
        "profile": e.profile.as_ref().map(|p| &p.0), "actor": e.actor.0, "action": e.action, "target": e.target,
        "args_sha256": e.args_sha256.as_ref().map(|h| hex(h)), "verdict": e.verdict,
    })
}

pub fn check(c: &AuditCheck) -> Value {
    json!({"entries": c.entries, "head": hex(&c.head), "broken_at": c.broken_at, "ok": c.broken_at.is_none()})
}

pub fn status(s: &ProfileStatus) -> Value {
    let mut runs = serde_json::Map::new();
    for (state, n) in &s.runs {
        runs.insert(run_state(*state).to_owned(), json!(n));
    }
    json!({
        "runs": runs, "oldest_ready_secs": s.oldest_ready_secs,
        "approvals_pending": s.approvals_pending, "holds": s.holds,
    })
}

pub fn listing<T>(l: &Listing<T>, one: impl Fn(&T) -> Value) -> Value {
    json!({"items": l.items.iter().map(one).collect::<Vec<_>>(), "next": l.next})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_time_are_plain() {
        assert_eq!(hex(&[0, 1, 0xab, 0xff]), "0001abff");
        assert_eq!(ms(SystemTime::UNIX_EPOCH), 0);
        assert_eq!(
            ms(SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_234)),
            1_234
        );
        assert!(ms(SystemTime::UNIX_EPOCH - std::time::Duration::from_millis(5)) < 0);
    }

    #[test]
    fn run_states_round_trip_through_their_words() {
        for s in [
            RunState::Ready,
            RunState::Leased,
            RunState::Waiting,
            RunState::Retry,
            RunState::Succeeded,
            RunState::Dead,
            RunState::Cancelled,
        ] {
            assert_eq!(parse_run_state(run_state(s)), Some(s));
        }
        assert_eq!(parse_run_state("Ready"), None);
        assert_eq!(parse_run_state(""), None);
    }
}
