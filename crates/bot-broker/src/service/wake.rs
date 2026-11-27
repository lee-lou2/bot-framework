//! Tool follow-up wake requests (`_meta["bot/wake"]`) committed atomically with call completion.
//!
//! - Allowed only for tools declaring `effects: true` and `schedule.create` in both pinned and current snapshots, invoked by an authorized human principal; wakes always return to the original conversation once at `at`.
//! - Always stripped from model-visible payloads; undeclared or error responses are ignored and audited.
//! - After side effects occur, any unconfirmable, invalid, or unauthorized wake holds the call unless ledger lookup proves commit already succeeded.

use super::{BrokerService, call::Flow};
use crate::{
    canon::canonicalize, json::parse, mcp::WAKE_KEY, resolve::find_tool,
    service::schedule::CREATE_TOOL,
};
use bot_core::{
    command::{CallResult, JsonPayload},
    ledger::{CallPhase, Outcome},
    port::Error,
    schedule::{MAX_INSTRUCTION_BYTES, Wake, utc::parse_utc},
    scope::authorize,
};
use serde_json::{Map, Value};
use std::{collections::BTreeSet, time::SystemTime};

pub(super) struct Extracted {
    /// Model-visible payload with `_meta["bot/wake"]` stripped (or untouched bytes if absent).
    pub payload: JsonPayload,
    pub request: Option<Result<Wake, ()>>,
    pub is_error: bool,
}

fn parse_request(value: &Value, now: SystemTime) -> Result<Wake, ()> {
    let map = value.as_object().ok_or(())?;
    let allowed: BTreeSet<&str> = ["at", "instruction", "quiet"].into();
    if map.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(());
    }
    let at = map
        .get("at")
        .and_then(Value::as_str)
        .and_then(parse_utc)
        .filter(|at| *at > now)
        .ok_or(())?;
    let instruction = map.get("instruction").and_then(Value::as_str).ok_or(())?;
    if instruction.trim().is_empty()
        || instruction.len() > MAX_INSTRUCTION_BYTES
        || instruction.contains('\0')
    {
        return Err(());
    }
    let quiet = match map.get("quiet") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(()),
    };
    Ok(Wake {
        at,
        instruction: instruction.to_owned(),
        quiet,
    })
}

/// Strips `_meta["bot/wake"]` (removing `_meta` when empty) while preserving other `_meta` keys.
pub(super) fn extract(payload: &JsonPayload, now: SystemTime) -> Extracted {
    let untouched = |payload: &JsonPayload| Extracted {
        payload: payload.clone(),
        request: None,
        is_error: false,
    };
    let Some(Value::Object(mut map)) = parse(&payload.0) else {
        return untouched(payload);
    };
    let is_error = matches!(map.get("isError"), Some(Value::Bool(true)));
    let Some(Value::Object(meta)) = map.get_mut("_meta") else {
        return untouched(payload);
    };
    let Some(requested) = meta.remove(WAKE_KEY) else {
        return untouched(payload);
    };
    if meta.is_empty() {
        map.remove("_meta");
    }
    let stripped = canonicalize(&Value::Object(Map::from_iter(map)));
    Extracted {
        payload: stripped.map_or_else(|| payload.clone(), JsonPayload),
        request: Some(parse_request(&requested, now)),
        is_error,
    }
}

impl BrokerService {
    pub(super) async fn finish_with_wake(
        &self,
        flow: &Flow,
        extracted: Extracted,
    ) -> Result<CallResult, Error> {
        let Extracted {
            payload,
            request,
            is_error,
        } = extracted;
        let hold = |reason: &'static str| async move {
            self.finish(flow, Outcome::Held(reason.into())).await?;
            Ok(CallResult::Held(flow.call.clone()))
        };
        let tool = &flow.resolved.tool;
        let declared = tool.effects && tool.scopes.contains(CREATE_TOOL);
        if !declared || is_error {
            let _ = self
                .note(
                    &flow.attempt.profile,
                    &flow.ctx.principal,
                    "call.wake",
                    &flow.tool,
                    Some(flow.sealed.sha256),
                    if declared {
                        "ignored_error"
                    } else {
                        "not_declared"
                    },
                )
                .await;
            self.finish(flow, Outcome::Done(payload.clone())).await?;
            self.still_current(flow).await?;
            return Ok(CallResult::Done(payload));
        }
        let Some(Ok(wake)) = request else {
            return hold("wake_invalid").await;
        };
        let Some(current) = self.parts.catalog.current(&flow.attempt.profile) else {
            return hold("wake_denied").await;
        };
        let scopes = BTreeSet::from([CREATE_TOOL.to_owned()]);
        let allowed = flow.ctx.principal.is_human()
            && authorize(&scopes, &flow.ctx, &current, &flow.pinned).is_ok();
        if !allowed {
            return hold("wake_denied").await;
        }
        // Re-check that the current revision still declares `effects` and `schedule.create` on this tool.
        let still_declared = find_tool(&current, &flow.ctx.agent, &flow.tool)
            .is_some_and(|tool| tool.effects && tool.scopes.contains(CREATE_TOOL));
        if !still_declared {
            return hold("wake_denied").await;
        }
        let committed = self
            .schedules
            .finish_with_wake(
                flow.attempt.clone(),
                flow.call.clone(),
                current.revision.clone(),
                payload.clone(),
                wake,
            )
            .await;
        match committed {
            Ok(_) => {
                self.still_current(flow).await?;
                Ok(CallResult::Done(payload))
            }
            Err(Error::Stale) => Err(Error::Stale),
            Err(_) => {
                // Store response may have been lost after commit: return stored result if done, else hold.
                let record = self
                    .parts
                    .ledger
                    .lookup(flow.attempt.clone(), flow.call.clone())
                    .await
                    .ok()
                    .flatten();
                match record.map(|record| record.phase) {
                    Some(CallPhase::Done(result)) => {
                        self.still_current(flow).await?;
                        Ok(CallResult::Done(result))
                    }
                    _ => hold("wake_unconfirmed").await,
                }
            }
        }
    }
}
