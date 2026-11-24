//! 모델 차례 체크포인트. 재개할 때 같은 호출 ID로 이어 가려고, 호출을 실행하기 **전에** 모델의 결정을 적는다.
//!
//! - `turn-<n>`: 모델이 정한 차례(말 + 호출 목록: ID·도구·인자). 호출 실행 전에 기록한다.
//! - `end-<n>`: 그 차례의 호출이 모두 끝난 교환(`Exchange`: 말 + 호출 결과). 이 키가 있으면 그 차례는 끝난 것이다.
//!
//! 위임 자식의 결과도 같은 체크포인트에 들어오지만(키는 위임 호출 ID) 여기서는 읽지 않는다.

use crate::model::{Exchange, PendingCall};
use bot_core::{
    command::{JsonPayload, Step},
    id::{CallId, ToolId},
};
use serde_json::{Value, json};
use std::collections::HashMap;

pub(super) fn plan_key(turn: usize) -> String {
    format!("turn-{turn}")
}

pub(super) fn end_key(turn: usize) -> String {
    format!("end-{turn}")
}

/// 모델이 정한 한 차례(아직 호출 결과가 없다).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Planned {
    pub text: String,
    pub calls: Vec<PendingCall>,
}

impl Planned {
    pub(super) fn to_payload(&self) -> JsonPayload {
        let calls: Vec<Value> = self
            .calls
            .iter()
            .map(|c| {
                let args: Value = serde_json::from_str(&c.args.0).unwrap_or(Value::Null);
                json!({"call": c.call.0, "tool": c.tool.0, "args": args})
            })
            .collect();
        JsonPayload(json!({"text": self.text, "calls": calls}).to_string())
    }

    pub(super) fn from_payload(payload: &JsonPayload) -> Option<Self> {
        let value: Value = serde_json::from_str(&payload.0).ok()?;
        let text = value.get("text")?.as_str()?.to_owned();
        let mut calls = Vec::new();
        for item in value.get("calls")?.as_array()? {
            calls.push(PendingCall {
                call: CallId(item.get("call")?.as_str()?.to_owned()),
                tool: ToolId(item.get("tool")?.as_str()?.to_owned()),
                args: JsonPayload(item.get("args")?.to_string()),
            });
        }
        Some(Self { text, calls })
    }
}

/// 체크포인트에서 되살린 상태: 끝난 차례들과, 호출 실행 중에 멈춘 차례(있으면).
#[derive(Debug, Default)]
pub(super) struct Restored {
    pub done: Vec<Exchange>,
    pub planned: Option<Planned>,
}

/// 손상된 체크포인트는 `None`이다(조용히 받아들이지 않는다).
pub(super) fn restore(steps: &[Step]) -> Option<Restored> {
    let by_key: HashMap<&str, &JsonPayload> =
        steps.iter().map(|s| (s.key.as_str(), &s.body)).collect();
    let mut restored = Restored::default();
    for turn in 0.. {
        if let Some(body) = by_key.get(end_key(turn).as_str()) {
            restored.done.push(Exchange::from_payload(body)?);
        } else if let Some(body) = by_key.get(plan_key(turn).as_str()) {
            restored.planned = Some(Planned::from_payload(body)?);
            break;
        } else {
            break;
        }
    }
    Some(restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CallEnd, DoneCall};

    fn planned() -> Planned {
        Planned {
            text: "찾아볼게요".into(),
            calls: vec![PendingCall {
                call: CallId("r:0:0".into()),
                tool: ToolId("agenda.list".into()),
                args: JsonPayload("{}".into()),
            }],
        }
    }

    fn step(key: String, body: JsonPayload) -> Step {
        Step { key, body }
    }

    #[test]
    fn a_planned_turn_round_trips() {
        let plan = planned();
        assert_eq!(Planned::from_payload(&plan.to_payload()), Some(plan));
    }

    #[test]
    fn finished_turns_come_back_in_order_and_a_half_done_turn_is_the_plan() {
        let end = Exchange {
            text: "ok".into(),
            calls: vec![DoneCall {
                call: CallId("r:0:0".into()),
                tool: ToolId("agenda.list".into()),
                args: JsonPayload("{}".into()),
                end: CallEnd::Done(JsonPayload(r#"{"events":[]}"#.into())),
            }],
        };
        let steps = vec![
            step(plan_key(0), planned().to_payload()),
            step(end_key(0), end.to_payload()),
            step(plan_key(1), planned().to_payload()),
            step(
                "r:1:0".into(),
                JsonPayload(r#"{"state":"succeeded","result":{}}"#.into()),
            ),
        ];
        let restored = restore(&steps).unwrap();
        assert_eq!(restored.done, vec![end]);
        assert_eq!(restored.planned, Some(planned()));
        assert!(restore(&[]).unwrap().planned.is_none());
    }

    #[test]
    fn damaged_checkpoints_are_rejected() {
        let bad = JsonPayload("{}".into());
        assert!(restore(&[step(plan_key(0), bad.clone())]).is_none());
        assert!(restore(&[step(end_key(0), bad)]).is_none());
    }
}
