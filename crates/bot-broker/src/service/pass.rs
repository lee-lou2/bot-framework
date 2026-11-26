//! F3: 직접 도구의 «처리하지 않음» 표식. 도구가 성공 결과 최상위 `_meta["bot/pass"] = true`와 빈 `content`만 돌려주면 이 이벤트를 처리하지 않았다는 뜻이고,
//! 호스트는 호출 결론·커서 전진(다음 후보 또는 기본 대화)·감사를 **한 커밋**으로 기록해(`CallLedger::finish_pass`) run을 대기열로 되돌린다.
//!
//! - 해석하는 곳: 일반 Slack 직접 트리거의 호출뿐(접수가 사슬을 남긴 run의 현재 후보 호출 ID). 민감 DM은 전용 원장이 별도로 판정한다. 모델·위임·HTTP·cron 호출의 표식은 떼어 낸다 —
//!   그 호출들의 결과는 표식이 없던 때와 바이트가 같다.
//! - 표식은 정확히 `{"content":[],"isError":false,"_meta":{"bot/pass":true}}`일 때만 유효하다. 본문·structuredContent·오류·wake·불리언 `true`가 아닌 값이
//!   함께 오면 `invalid_pass`로 그 후보에서 닫는다(다음 실행 0).
//! - 판별용 읽기만 한 호출만 넘길 수 있다: 쓰기 등급인데 단계 원장(`effects`)이 없는 도구는 부작용을 가릴 수 없어 거절하고, 효과 단계·회신·승인이 이미 있으면 저장소가
//!   같은 잠금 안에서 거절한다(`pass_after_effect`). 거절·무효는 `Ran::Blocked`와 같이 닫는다(읽기는 거절, 그 밖은 보류).

use super::{BrokerService, call::Flow};
use crate::{canon::canonicalize, classify::Class, json::parse, mcp::PASS_KEY};
use bot_core::{
    command::{CallResult, JsonPayload, Target},
    id::AgentId,
    ledger::{FinishCall, Outcome},
    pass::{INVALID_PASS, PASS_AFTER_EFFECT, PassNext, direct_call_id},
    port::Error,
    profile::Snapshot,
    scope::{RUN_SCOPE, effective},
    state::Risk,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// 결과에서 떼어 낸 표식
pub(crate) struct Marker {
    /// 규칙에 맞는 표식인가
    pub valid: bool,
}

/// 결과에서 `_meta["bot/pass"]`를 떼어 낸다(다른 `_meta` 항목은 그대로, 비면 `_meta`도 뺀다). 표식이 없으면 결과는 바이트 그대로다.
pub(crate) fn split(payload: &JsonPayload) -> (JsonPayload, Option<Marker>) {
    let Some(Value::Object(mut map)) = parse(&payload.0) else {
        return (payload.clone(), None);
    };
    let Some(Value::Object(meta)) = map.get_mut("_meta") else {
        return (payload.clone(), None);
    };
    let Some(marker) = meta.remove(PASS_KEY) else {
        return (payload.clone(), None);
    };
    // 표식 말고 남는 것이 있으면(본문·structuredContent·오류·wake 등) 유효하지 않다
    let alone = meta.is_empty()
        && !map.contains_key("structuredContent")
        && map.get("isError") == Some(&Value::Bool(false))
        && map
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty);
    let valid = marker == Value::Bool(true) && alone;
    if map
        .get("_meta")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        map.remove("_meta");
    }
    let stripped = canonicalize(&Value::Object(map));
    (
        stripped.map_or_else(|| payload.clone(), JsonPayload),
        Some(Marker { valid }),
    )
}

/// 이 결과의 `_meta`에 표식이 있는가(민감 DM 전용 경로의 넘김·보류 판정에 쓴다).
pub fn carries_marker(payload: &str) -> bool {
    matches!(parse(payload), Some(Value::Object(map))
        if map.get("_meta").and_then(Value::as_object).is_some_and(|meta| meta.contains_key(PASS_KEY)))
}

impl BrokerService {
    /// 이 호출의 표식을 해석해도 되는가: 접수가 사슬을 남긴 run의 현재 후보를 직접 호출하는 그 호출 ID다(모델·위임이 부른 호출은 아니다).
    pub(super) fn pass_position(flow: &Flow) -> Option<u32> {
        let pos = flow.ctx.work.pass_pos?;
        (matches!(flow.ctx.work.target, Target::Tool { .. })
            && flow.call == direct_call_id(&flow.attempt.run, pos))
        .then_some(pos)
    }

    /// 표식이 붙은 결과를 마무리한다. 유효하고 넘길 수 있으면 저장소가 한 커밋으로 넘기고 `Passed`다. 아니면 호출을 닫는다.
    pub(super) async fn finish_with_pass(
        &self,
        flow: &Flow,
        class: &Class,
        stripped: JsonPayload,
        marker: Marker,
        pos: u32,
    ) -> Result<CallResult, Error> {
        if !marker.valid {
            return self.close_pass(flow, class, INVALID_PASS).await;
        }
        // 부작용을 가릴 수 없는 도구(쓰기 등급인데 단계 원장이 없다)는 넘길 수 없다. 읽기 도구와 단계 원장을 쓰는 도구만 저장소 확인으로 간다.
        if class.risk != Risk::Read && !flow.resolved.tool.effects {
            return self.close_pass(flow, class, PASS_AFTER_EFFECT).await;
        }
        let next = self
            .parts
            .ledger
            .finish_pass(
                FinishCall {
                    attempt: flow.attempt.clone(),
                    call: flow.call.clone(),
                    outcome: Outcome::Done(stripped),
                },
                pos,
                self.runnable_agents(flow),
            )
            .await?;
        match next {
            PassNext::Refused(why) => self.close_pass(flow, class, why).await,
            PassNext::Candidate(_) | PassNext::Default | PassNext::End => Ok(CallResult::Passed),
        }
    }

    /// **지금** 요청자가 `run`을 쓸 수 있는 에이전트들(기본 대화가 시작해도 되는 곳): 접수가 고정한 범위(`ctx.scopes`)에 `run`이 있고, 현재 판본과 실행이 고정한 판본 **둘 다**에서
    /// (역할 ∩ 정책 ∩ 에이전트 상한)에 `run`이 있어야 한다(위임과 같은 규칙, `delegation_scopes`). 현재 판본은 이 호출을 시작한 뒤 바뀌었을 수 있어 카탈로그에서 다시 읽는다.
    fn runnable_agents(&self, flow: &Flow) -> BTreeSet<AgentId> {
        let Some(current) = self.parts.catalog.current(&flow.attempt.profile) else {
            return BTreeSet::new();
        };
        if !flow.ctx.scopes.contains(RUN_SCOPE) {
            return BTreeSet::new();
        }
        let allowed = |snapshot: &Snapshot, agent: &AgentId| {
            effective(snapshot, &flow.ctx.principal, agent)
                .is_some_and(|scopes| scopes.contains(RUN_SCOPE))
        };
        current
            .agents
            .keys()
            .filter(|agent| allowed(&current, agent) && allowed(&flow.pinned, agent))
            .cloned()
            .collect()
    }

    /// 넘길 수 없는 호출을 닫는다(`Ran::Blocked`와 같은 규칙): 도구는 이미 실행됐다. 읽기는 외부 효과가 없으니 거절로, 그 밖은 효과가 불명이라 보류한다.
    /// 고정 사유를 감사에 남기고 다음 후보·기본 대화는 부르지 않는다.
    async fn close_pass(
        &self,
        flow: &Flow,
        class: &Class,
        why: &'static str,
    ) -> Result<CallResult, Error> {
        self.note(
            &flow.attempt.profile,
            &flow.ctx.principal,
            "call.pass_refused",
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(value: Value) -> JsonPayload {
        JsonPayload(canonicalize(&value).unwrap())
    }

    #[test]
    fn only_the_exact_empty_marker_is_valid_and_it_is_always_stripped() {
        let (stripped, marker) = split(&payload(
            json!({"content": [], "isError": false, "_meta": {"bot/pass": true}}),
        ));
        assert!(marker.unwrap().valid);
        assert_eq!(stripped.0, r#"{"content":[],"isError":false}"#);
        for bad in [
            json!({"content": [{"type": "text", "text": "x"}], "isError": false, "_meta": {"bot/pass": true}}),
            json!({"content": [], "isError": true, "_meta": {"bot/pass": true}}),
            json!({"content": [], "isError": false, "structuredContent": {}, "_meta": {"bot/pass": true}}),
            json!({"content": [], "isError": false, "_meta": {"bot/pass": true, "bot/wake": {"at": "x"}}}),
            // 표식과 함께 온 다른 `_meta` 항목(파서가 남기는 표지)
            json!({"content": [], "isError": false, "_meta": {"bot/pass": true, "bot/extra": true}}),
            json!({"content": [], "isError": false, "_meta": {"bot/pass": "true"}}),
            json!({"content": [], "isError": false, "_meta": {"bot/pass": 1}}),
            json!({"content": [], "isError": false, "_meta": {"bot/pass": false}}),
        ] {
            let (stripped, marker) = split(&payload(bad.clone()));
            assert!(!marker.expect("표식은 있다").valid, "{bad}");
            assert!(!stripped.0.contains("bot/pass"), "{bad}");
        }
    }

    #[test]
    fn results_without_the_marker_keep_their_bytes() {
        for plain in [
            json!({"content": [], "isError": false}),
            json!({"content": [{"type": "text", "text": "{\"_meta\":{\"bot/pass\":true}}"}], "isError": false}),
            json!({"content": [{"type": "text", "text": "true"}], "isError": false, "structuredContent": {"bot/pass": true}}),
        ] {
            let before = payload(plain);
            let (after, marker) = split(&before);
            assert!(marker.is_none());
            assert_eq!(after, before);
        }
        assert!(!carries_marker(r#"{"content":[],"isError":false}"#));
        assert!(carries_marker(
            r#"{"_meta":{"bot/pass":true},"content":[],"isError":false}"#
        ));
    }
}
