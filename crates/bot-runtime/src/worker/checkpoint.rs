//! 모델 차례 체크포인트. 재개할 때 같은 호출 ID로 이어 가려고, 호출을 실행하기 **전에** 모델의 결정을 적는다.
//!
//! - `turn-<n>`: 모델이 정한 차례(말 + 호출 목록: ID·도구·인자 + 그 응답의 사용량 기록 + 불투명 공급자 상태). 호출 실행 전에 기록한다.
//! - `cli-refusal-<호출 ID>`: CLI 프런트와 같은 확정 거절 단어. 계획 중 해당 위치를 다시 처리할 때 `worker::calls`가 읽는다.
//! - `end-<n>`: 그 차례의 호출이 모두 끝난 교환(`Exchange`: 말 + 호출 결과). 이 키가 있으면 그 차례는 끝난 것이다.
//!
//! 위임 자식의 결과도 같은 체크포인트에 들어오지만(키는 위임 호출 ID) 여기서는 읽지 않는다.

use crate::model::{Exchange, PendingCall};
use bot_core::{
    command::{JsonPayload, Step, TokenUsage},
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

/// 한 차례를 정한 모델 응답의 사용량 기록. 합계는 모든 차례가 `Reported`일 때만 낸다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TurnUsage {
    /// 공급자가 이 요청의 입력·출력을 실제로 보고했다.
    Reported(TokenUsage),
    /// 공급자가 보고하지 않았다(없는 값을 0으로 채우지 않는다).
    Unreported,
    /// 기록이 없다(`usage` 키가 없는 옛 체크포인트·손상): 합계의 근거가 없다.
    Unknown,
}

/// 모델이 정한 한 차례(아직 호출 결과가 없다).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Planned {
    pub text: String,
    pub calls: Vec<PendingCall>,
    pub usage: TurnUsage,
    /// 불투명 공급자 상태(`ModelStep::Calls::state`). 워커는 해석하지 않는다.
    pub state: Option<String>,
}

impl Planned {
    /// 본문: `{"text","calls":[…],"usage":{"prompt","completion"}|null,"state":"…"}`. `usage`는 보고됐으면 객체, 보고되지 않았으면 `null`이고 키가 없으면 «기록 없음»이다.
    pub(super) fn to_payload(&self) -> JsonPayload {
        let calls: Vec<Value> = self
            .calls
            .iter()
            .map(|c| {
                let args: Value = serde_json::from_str(&c.args.0).unwrap_or(Value::Null);
                json!({"call": c.call.0, "tool": c.tool.0, "args": args})
            })
            .collect();
        let mut body = json!({"text": self.text, "calls": calls});
        match self.usage {
            TurnUsage::Reported(usage) => {
                body["usage"] = json!({"prompt": usage.prompt, "completion": usage.completion});
            }
            TurnUsage::Unreported => body["usage"] = Value::Null,
            TurnUsage::Unknown => {}
        }
        if let Some(state) = &self.state {
            body["state"] = Value::String(state.clone());
        }
        JsonPayload(body.to_string())
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
        let usage = match value.get("usage") {
            None => TurnUsage::Unknown,
            Some(Value::Null) => TurnUsage::Unreported,
            Some(reported) => TurnUsage::Reported(TokenUsage {
                prompt: reported.get("prompt")?.as_u64()?,
                completion: reported.get("completion")?.as_u64()?,
            }),
        };
        let state = match value.get("state") {
            None | Some(Value::Null) => None,
            Some(state) => Some(state.as_str()?.to_owned()),
        };
        Some(Self {
            text,
            calls,
            usage,
            state,
        })
    }
}

/// 체크포인트에서 되살린 상태: 끝난 차례들과, 호출 실행 중에 멈춘 차례(있으면).
#[derive(Debug, Default)]
pub(super) struct Restored {
    pub done: Vec<Exchange>,
    pub planned: Option<Planned>,
    /// 차례마다 하나(끝난 차례들 + 멈춘 차례): 그 차례를 정한 응답의 사용량 기록. 계획(`turn-<n>`) 없이 끝난 차례는 `Unknown`이다.
    pub usages: Vec<TurnUsage>,
}

/// 손상된 체크포인트는 `None`이다(조용히 받아들이지 않는다).
pub(super) fn restore(steps: &[Step]) -> Option<Restored> {
    let by_key: HashMap<&str, &JsonPayload> =
        steps.iter().map(|s| (s.key.as_str(), &s.body)).collect();
    let mut restored = Restored::default();
    for turn in 0.. {
        let plan = by_key.get(plan_key(turn).as_str());
        if let Some(body) = by_key.get(end_key(turn).as_str()) {
            restored.done.push(Exchange::from_payload(body)?);
            // 끝난 차례의 계획은 사용량만 본다: 읽을 수 없으면 기록 없음이다(차례 자체는 `end-<n>`이 정한다)
            restored.usages.push(
                plan.and_then(|body| Planned::from_payload(body))
                    .map_or(TurnUsage::Unknown, |planned| planned.usage),
            );
        } else if let Some(body) = plan {
            let planned = Planned::from_payload(body)?;
            restored.usages.push(planned.usage);
            restored.planned = Some(planned);
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
            usage: TurnUsage::Reported(TokenUsage {
                prompt: 11,
                completion: 4,
            }),
            state: Some(r#"{"v":1}"#.into()),
        }
    }

    fn step(key: String, body: JsonPayload) -> Step {
        Step { key, body }
    }

    #[test]
    fn a_planned_turn_round_trips() {
        let plan = planned();
        assert_eq!(Planned::from_payload(&plan.to_payload()), Some(plan));
        // 보고되지 않은 사용량과 상태 없음도 그대로 돌아온다
        let bare = Planned {
            usage: TurnUsage::Unreported,
            state: None,
            ..planned()
        };
        assert_eq!(Planned::from_payload(&bare.to_payload()), Some(bare));
    }

    #[test]
    fn the_usage_record_tells_reported_from_unreported_from_no_record() {
        let body = |usage: &str| JsonPayload(format!(r#"{{"text":"x","calls":[]{usage}}}"#));
        let usage_of = |usage: &str| Planned::from_payload(&body(usage)).unwrap().usage;
        // 키가 없는 옛 체크포인트는 근거가 없다
        assert_eq!(usage_of(""), TurnUsage::Unknown);
        assert_eq!(usage_of(r#","usage":null"#), TurnUsage::Unreported);
        assert_eq!(
            usage_of(r#","usage":{"prompt":3,"completion":2}"#),
            TurnUsage::Reported(TokenUsage {
                prompt: 3,
                completion: 2
            })
        );
        // 모양이 어긋난 기록은 손상이다(조용히 «없음»으로 읽지 않는다)
        for bad in [
            r#","usage":{"prompt":3}"#,
            r#","usage":"many""#,
            r#","usage":[1]"#,
        ] {
            assert_eq!(Planned::from_payload(&body(bad)), None, "{bad}");
        }
        assert_eq!(
            Planned::from_payload(&body(r#","state":{"a":1}"#)),
            None,
            "문자열이 아닌 상태"
        );
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
            state: None,
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
        // 차례마다 사용량 기록이 하나씩이다(끝난 차례 + 멈춘 차례)
        assert_eq!(restored.usages, vec![planned().usage, planned().usage]);
        assert!(restore(&[]).unwrap().planned.is_none());
        assert!(restore(&[]).unwrap().usages.is_empty());
        // 계획 없이 끝난 차례(옛 형식·손상)는 기록 없음이다
        let orphan = restore(&[step(end_key(0), Exchange::default().to_payload())]).unwrap();
        assert_eq!(orphan.usages, vec![TurnUsage::Unknown]);
    }

    #[test]
    fn damaged_checkpoints_are_rejected() {
        let bad = JsonPayload("{}".into());
        assert!(restore(&[step(plan_key(0), bad.clone())]).is_none());
        assert!(restore(&[step(end_key(0), bad)]).is_none());
    }
}
