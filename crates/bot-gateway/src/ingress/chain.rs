//! 넘김(`bot/pass`) 후보 사슬을 만든다: 접수가 이 사건에 맞는 직접 도구 트리거를 order 순으로 고정하고, 마지막 후보까지 넘기면 갈 기본 대화를 정한다.
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

/// 이 사건에 맞는 트리거를 order 순으로 보아 **직접 도구 호출**만 후보로 삼고, 처음 만나는 에이전트 지시 트리거는 기본 대화가 된다(그 뒤는 닿지 않는다).
/// 지시 트리거가 없으면 기본 대화는 원래 바인딩의 에이전트가 받는 사건 입력이다: 멘션·답글은 사람이 쓴 글 그대로, 반응은 사건의 고정 필드만으로 만든 고정 형식이다(`reaction_event`).
/// 기본 대화는 에이전트 실행이라 요청자가 `run` 스코프를 가져야 한다(접수는 첫 후보인 도구만 보고 이 스코프를 요구하지 않았다): 없으면 기본 대화 없이 후보만 남는다.
/// 첫 후보가 이번 접수의 대상이 아니면(첫 일치가 에이전트 지시) 사슬은 없다. 입력 매핑이 틀린 후보는 건너뛴다.
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

/// 지시 트리거가 없을 때 기본 에이전트가 받는 입력. 멘션·답글은 글 그대로, 반응은 고정 형식이다.
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

/// 반응 사건의 고정 형식 입력(`{"event":"reaction_added","item_ts":…,"reaction":…}`): 검증한 반응 이름과 대상 글의 ts만 담는다.
/// 원글 본문·작성자·자유 문장은 싣지 않는다. 모양이 맞지 않으면 만들지 않는다(접수 앞단이 이미 같은 검사를 했지만 여기서도 본다).
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
