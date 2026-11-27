//! 도구의 후속 확인 요청: 신뢰 도구가 성공 응답의 최상위 `_meta["bot/wake"] = {at, instruction, quiet?}`로 한 번짜리 예약을
//! 요청할 수 있다.
//!
//! - 요청할 수 있는 도구: `effects: true`이고 `schedule.create` 스코프를 선언한 도구뿐이다. 요청자는 지금도 사람이며 같은 스코프를
//!   현재·고정 판본에서 가져야 한다(예약 주체는 인증된 사람이다). 예약은 항상 원래 대화로 한 번(`at`)만 가고 다른 수신자는 없다.
//! - 결과와 예약은 `UserSchedules::finish_with_wake`가 한 커밋으로 저장한다(같은 논리 호출의 재전달은 예약이 한 번뿐이다).
//! - 이 메타데이터는 모델이 보는 결과에서 항상 제거한다(요청할 수 없는 도구가 보내도 제거하고 감사에 남긴다).
//! - 이미 효과를 낸 뒤 예약을 확정할 수 없으면(요청이 틀렸거나 권한이 없어졌거나 한도·저장소 오류) 호출을 보류한다.
//!   저장소 응답을 잃어 커밋 여부를 모르면 기록을 먼저 보고, 완료돼 있으면 그 결과를 돌려준다.
//! - 민감 DM 전용 경로에는 이 기능이 없다(그 경로의 결과는 저장하지도 돌려주지도 않는다).

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

/// 결과에서 떼어 낸 후속 확인 요청.
pub(super) struct Extracted {
    /// 모델이 볼 결과(`_meta["bot/wake"]`를 뺀 것. 요청이 없으면 바이트 그대로다).
    pub payload: JsonPayload,
    /// `None`: 요청 없음. `Some(Err)`: 요청했지만 읽을 수 없다. `Some(Ok)`: 읽은 요청.
    pub request: Option<Result<Wake, ()>>,
    /// 도구가 오류로 끝냈다고 알렸다(`isError: true`). 후속 확인은 성공 응답에서만 받는다.
    pub is_error: bool,
}

/// `{at, instruction, quiet?}`만 받는다: `at`은 UTC RFC 3339로 지금보다 뒤, 지시문은 비어 있지 않고 8 KiB 이하다.
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

/// 결과에서 `_meta["bot/wake"]`를 떼어 낸다. 다른 `_meta` 항목은 그대로 두고, `_meta`가 비면 함께 뺀다.
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
    /// 후속 확인을 요청한 결과를 마무리한다(도구는 이미 실행됐다). 선언한 도구가 확정하지 못하면 보류한다.
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
        // 요청할 수 없는 도구(선언이 없다)나 오류 결과: 요청은 버리고 결과만 돌려준다. 감사에 남긴다.
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
        // 선언한 도구: 이미 효과를 냈으니 예약을 확정할 수 없으면 보류한다.
        let Some(Ok(wake)) = request else {
            return hold("wake_invalid").await;
        };
        // 현재 권한: 방금 다시 읽은 현재 판본과 실행이 고정한 판본에서 요청자가 `schedule.create`를 가져야 한다.
        let Some(current) = self.parts.catalog.current(&flow.attempt.profile) else {
            return hold("wake_denied").await;
        };
        let scopes = BTreeSet::from([CREATE_TOOL.to_owned()]);
        let allowed = flow.ctx.principal.is_human()
            && authorize(&scopes, &flow.ctx, &current, &flow.pinned).is_ok();
        if !allowed {
            return hold("wake_denied").await;
        }
        // 시작 때의 선언만으로는 부족하다: 지금 판본의 같은 도구도 `effects`와 `schedule.create` 선언을 그대로 두어야 한다
        // (철회됐거나 도구가 없어졌으면 확정하지 못하니 보류한다).
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
            // claim을 잃었다: 아무것도 커밋되지 않았다(다른 claim이 이어 간다).
            Err(Error::Stale) => Err(Error::Stale),
            Err(_) => {
                // 응답을 잃었을 수 있다: 이미 커밋돼 있으면 그 결과를 돌려준다. 아니면 확정하지 못했으니 보류한다.
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
