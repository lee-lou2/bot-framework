//! 조회 값 → 응답 JSON. 시각은 유닉스 밀리초, 해시는 소문자 16진, 상태는 소문자 고정 낱말이다.
//! 계약 타입은 serde에 의존하지 않으므로 여기서 손으로 옮긴다. 여기 없는 값은 응답에 나가지 않는다.

use crate::port::*;
use bot_core::{
    command::memory_share_hash,
    id::MemoryId,
    schedule::{DELIVER_TOOL, DELIVERY_CARD_LABEL},
    state::{ConversationKind, EffectState, Risk, RunState},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fmt::Write, time::SystemTime};

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// 사전 승인 한 행: 문자열 4개·정수 3개·불리언 1개(관리 화면과 계약).
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

/// `?state=` 값: 대기(기본)·처리됨(대기를 벗어난 모두, 자동 만료·대체 포함)·전체.
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

/// 이 run 현재 시도의 추적 ID(시도에서 결정적으로 계산한 값).
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
        "lineage": d.lineage.iter().map(run).collect::<Vec<_>>(),
        "children": d.children.iter().map(run).collect::<Vec<_>>(),
        "calls": d.calls.iter().map(call).collect::<Vec<_>>(),
        "effects": d.effects.iter().map(effect).collect::<Vec<_>>(),
    })
}

/// 보류의 종류 낱말. 판정 «미실행»의 결과가 종류마다 다르다: `private`(민감 DM)는 본문이 없어 다시 실행하지 못하고 종료하며,
/// `reply`(게이트웨이 회신)는 같은 회신을 다시 보내고, `tool`(도구 호출)은 같은 호출을 다시 이어 간다.
/// 저장소가 조회한 출처(도구 행·회신 행·민감 접수 행)가 채운 `kind`만 따른다. 도구 이름이나 호출 ID의 모양은 보지 않는다.
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

/// 카드 본문에 요청자 본인만 볼 수 있는 글이 담기는 승인의 도구: 기억 공유(비공개 기억 본문)와 예약 전송(다른 수신자에게 나갈 최종 회신 초안).
pub const OWNER_ONLY_BODY_TOOLS: [&str; 2] = ["memory.share", DELIVER_TOOL];

/// 카드 본문(`render`)을 이 조회자에게 보여도 되는가. 소유자만 보는 카드는 요청자이자 승인자인 본인(기억의 소유자, 예약을 만든 사람)이
/// 인증된 경우에만 본문을 낸다. 운영자도 예외가 아니다(메타데이터·감사는 그대로 본다). 요청자와 승인자가 다르면 아무도 못 본다.
pub fn body_visible(a: &ApprovalView, viewer: &Viewer) -> bool {
    !OWNER_ONLY_BODY_TOOLS.contains(&a.tool.0.as_str())
        || (viewer.principal == a.requester && viewer.principal == a.approver)
}

/// 기억 공유 카드가 «기억 ID와 본문 전체»를 보이고, 그것이 승인 지문(`args_sha256`)과 일치하는가.
/// 카드에는 `기억 ID`와 `본문` 행이 각각 정확히 하나여야 한다(행이 없거나 둘 이상이면 모호하므로 일치하지 않는다).
/// `본문` 행은 이어진 줄을 모두 이은 한 행이고, 원문 그대로이거나 JSON 문자열(따옴표 포함)이어도 된다.
/// 제목·공유 범위만 남은 카드, 본문 일부가 빠진 카드, 다른 기억의 지문을 가진 카드는 일치하지 않는다.
fn share_card_matches(a: &ApprovalView) -> bool {
    let only = |label: &str| {
        let mut rows = a.render.iter().filter(|(k, _)| k == label);
        match (rows.next(), rows.next()) {
            (Some((_, v)), None) => Some(v.as_str()),
            _ => None,
        }
    };
    let (Some(id), Some(body)) = (only("기억 ID"), only("본문")) else {
        return false;
    };
    let id = MemoryId(id.to_owned());
    let hash_of = |text: &str| memory_share_hash(&id, &Sha256::digest(text.as_bytes()).into());
    hash_of(body) == a.args_sha256
        || serde_json::from_str::<String>(body).is_ok_and(|text| hash_of(&text) == a.args_sha256)
}

/// 예약 전송 카드가 «전송 초안 전체»를 한 행으로 보이고, 그 원문의 SHA-256이 승인 지문(`args_sha256`)과 같은가.
/// `전송 초안` 행은 정확히 하나여야 하고(없거나 둘 이상이면 모호하다) 값은 이어진 조각을 모두 이은 원문 그대로여야 한다:
/// 승인자가 보는 글이 곧 지문의 입력이다. 일부만 보이는 카드·글자가 바뀐 카드·다른 초안의 지문을 가진 카드는 일치하지 않는다.
fn delivery_card_matches(a: &ApprovalView) -> bool {
    let mut rows = a.render.iter().filter(|(k, _)| k == DELIVERY_CARD_LABEL);
    match (rows.next(), rows.next()) {
        (Some((_, draft)), None) => {
            <[u8; 32]>::from(Sha256::digest(draft.as_bytes())) == a.args_sha256
        }
        _ => false,
    }
}

/// 승인(긍정 판정)을 받아도 되는 카드인가. 소유자 전용 본문이 있는 카드는 승인하는 사람이 **승인 지문과 일치하는 내용 전체**
/// (기억 공유는 기억 ID·본문, 예약 전송은 전송 초안)를 실제로 볼 수 있어야 한다: 소유자가 아니거나, 카드에 제목·공유 범위만 있거나,
/// 본문 일부가 빠졌거나, 지문과 다르면 승인할 수 없다(볼 수 없는 내용을 승인하게 되므로). 거절은 늘 허용한다. 화면과 판정 API가 같은 조건을 쓴다.
pub fn approve_blocked(a: &ApprovalView, viewer: &Viewer) -> Option<&'static str> {
    let owner_only = OWNER_ONLY_BODY_TOOLS.contains(&a.tool.0.as_str());
    let complete = match a.tool.0.as_str() {
        "memory.share" => share_card_matches(a),
        DELIVER_TOOL => delivery_card_matches(a),
        _ => !a.render.is_empty(),
    };
    (owner_only && (!body_visible(a, viewer) || !complete)).then_some("no_body")
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
