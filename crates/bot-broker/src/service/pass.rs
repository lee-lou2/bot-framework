//! Direct-tool pass (`bot/pass`) handling that advances trigger chains when a tool declines an event without side effects.
//!
//! Interpreted only for Slack direct-trigger candidate calls (private DM uses its own ledger; all other
//! callers strip the marker transparently). Valid only as `{"content":[],"isError":false,"_meta":{"bot/pass":true}}`
//! on read tools or effect-journaled tools with no recorded effects, replies, or approvals.

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

pub(crate) struct Marker {
    pub valid: bool,
}

/// Strips `_meta["bot/pass"]` (dropping `_meta` if empty) and classifies validity; payloads without the marker are returned byte-identical.
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

pub fn carries_marker(payload: &str) -> bool {
    matches!(parse(payload), Some(Value::Object(map))
        if map.get("_meta").and_then(Value::as_object).is_some_and(|meta| meta.contains_key(PASS_KEY)))
}

impl BrokerService {
    /// Returns the chain cursor only when this call is the current direct-trigger candidate call ID.
    pub(super) fn pass_position(flow: &Flow) -> Option<u32> {
        let pos = flow.ctx.work.pass_pos?;
        (matches!(flow.ctx.work.target, Target::Tool { .. })
            && flow.call == direct_call_id(&flow.attempt.run, pos))
        .then_some(pos)
    }

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
        // Non-read tools without an effect journal cannot prove absence of side effects.
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

    /// Agents the requester currently holds `run` for across accepted scopes, current snapshot, and pinned snapshot.
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

    /// Mirrors `Ran::Blocked`: read calls fail as `Denied`, while write calls become `Held` because effects are unknown.
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
            json!({"content": [], "isError": false, "_meta": {"bot/pass": true, "bot/extra": true}}),
            json!({"content": [], "isError": false, "_meta": {"bot/pass": "true"}}),
            json!({"content": [], "isError": false, "_meta": {"bot/pass": 1}}),
            json!({"content": [], "isError": false, "_meta": {"bot/pass": false}}),
        ] {
            let (stripped, marker) = split(&payload(bad.clone()));
            assert!(!marker.expect("marker expected").valid, "{bad}");
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
