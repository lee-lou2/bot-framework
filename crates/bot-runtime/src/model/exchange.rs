//! 이번 run에서 이미 한 모델 차례와 호출 결과. 워커가 체크포인트(`Step.body`)로 적고 재개 때 되살린다.

use bot_core::{
    command::JsonPayload,
    id::{CallId, ToolId},
};
use serde_json::{Value, json};

/// 호출 하나의 결말. 승인 대기·보류는 run이 멈춰 있는 동안이라 모델에게 보이지 않는다(풀린 뒤의 결과만 들어온다).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallEnd {
    Done(JsonPayload),
    /// 고정 분류 코드(오류 원문 금지)
    Failed(String),
    /// 승인이 거절·만료됐거나 정책이 막았다.
    Denied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoneCall {
    pub call: CallId,
    pub tool: ToolId,
    pub args: JsonPayload,
    pub end: CallEnd,
}

/// 모델 한 차례와 그 차례가 요청한 호출들의 결과.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Exchange {
    pub text: String,
    pub calls: Vec<DoneCall>,
    /// 실행기가 이 차례를 정할 때 남긴 **불투명 공급자 상태**(`ModelStep::Calls::state`). 워커는 해석하지 않고 체크포인트에 그대로 적었다가 다음 요청의 같은 차례에
    /// 돌려준다(공급자가 도구 호출과 함께 준 서명·원래 호출 ID를 다음 요청에 되돌려야 할 때). 실행기가 만든 글이라 신뢰하지 않고, 같은 종류의 실행기만 읽는다.
    pub state: Option<String>,
}

fn json_of(payload: &JsonPayload) -> Value {
    serde_json::from_str(&payload.0).unwrap_or(Value::Null)
}

impl Exchange {
    /// 체크포인트 본문. `{"text":…,"calls":[{"call","tool","args","end":{"done":…}|{"failed":"코드"}|{"denied":true}}],"state":"…"}`(`state`는 있을 때만)
    pub fn to_payload(&self) -> JsonPayload {
        let calls: Vec<Value> = self
            .calls
            .iter()
            .map(|c| {
                let end = match &c.end {
                    CallEnd::Done(result) => json!({"done": json_of(result)}),
                    CallEnd::Failed(code) => json!({"failed": code}),
                    CallEnd::Denied => json!({"denied": true}),
                };
                json!({"call": c.call.0, "tool": c.tool.0, "args": json_of(&c.args), "end": end})
            })
            .collect();
        let mut body = json!({"text": self.text, "calls": calls});
        if let Some(state) = &self.state {
            body["state"] = Value::String(state.clone());
        }
        JsonPayload(body.to_string())
    }

    /// `to_payload`의 역. 모양이 다르면 None(손상된 체크포인트를 조용히 받아들이지 않는다).
    pub fn from_payload(payload: &JsonPayload) -> Option<Exchange> {
        let value: Value = serde_json::from_str(&payload.0).ok()?;
        let text = value.get("text")?.as_str()?.to_owned();
        let mut calls = Vec::new();
        for item in value.get("calls")?.as_array()? {
            let end = item.get("end")?;
            let end = if let Some(done) = end.get("done") {
                CallEnd::Done(JsonPayload(done.to_string()))
            } else if let Some(code) = end.get("failed") {
                CallEnd::Failed(code.as_str()?.to_owned())
            } else if end.get("denied").is_some() {
                CallEnd::Denied
            } else {
                return None;
            };
            calls.push(DoneCall {
                call: CallId(item.get("call")?.as_str()?.to_owned()),
                tool: ToolId(item.get("tool")?.as_str()?.to_owned()),
                args: JsonPayload(item.get("args")?.to_string()),
                end,
            });
        }
        let state = match value.get("state") {
            None | Some(Value::Null) => None,
            Some(state) => Some(state.as_str()?.to_owned()),
        };
        Some(Exchange { text, calls, state })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Exchange {
        Exchange {
            text: "확인해 볼게요".into(),
            calls: vec![
                DoneCall {
                    call: CallId("r-1:0:0".into()),
                    tool: ToolId("agenda.list".into()),
                    args: JsonPayload("{}".into()),
                    end: CallEnd::Done(JsonPayload(
                        r#"{"events":[{"id":"e1","title":"산책"}]}"#.into(),
                    )),
                },
                DoneCall {
                    call: CallId("r-1:0:1".into()),
                    tool: ToolId("agenda.delete".into()),
                    args: JsonPayload(r#"{"event":"e1"}"#.into()),
                    end: CallEnd::Denied,
                },
                DoneCall {
                    call: CallId("r-1:0:2".into()),
                    tool: ToolId("agenda.list".into()),
                    args: JsonPayload("{}".into()),
                    end: CallEnd::Failed("unavailable".into()),
                },
            ],
            state: Some(r#"{"v":1}"#.into()),
        }
    }

    #[test]
    fn payload_round_trips() {
        let original = sample();
        assert_eq!(
            Exchange::from_payload(&original.to_payload()),
            Some(original)
        );
    }

    #[test]
    fn the_provider_state_is_kept_verbatim_and_a_payload_without_it_has_none() {
        let mut exchange = sample();
        exchange.state = Some("{\"a\":\"줄\\n바꿈\"}".into());
        let back = Exchange::from_payload(&exchange.to_payload()).unwrap();
        assert_eq!(back.state, exchange.state);
        // 상태가 없는 교환의 본문에는 `state` 키가 없고, 옛 본문(키 없음)은 상태 없음으로 읽힌다
        exchange.state = None;
        assert!(!exchange.to_payload().0.contains("state"));
        let old = JsonPayload(r#"{"text":"x","calls":[]}"#.into());
        assert_eq!(Exchange::from_payload(&old).unwrap().state, None);
        // 문자열이 아닌 상태는 손상이다
        let bad = JsonPayload(r#"{"text":"x","calls":[],"state":{"a":1}}"#.into());
        assert_eq!(Exchange::from_payload(&bad), None);
    }

    #[test]
    fn payload_is_independent_of_json_layout() {
        let a = JsonPayload(r#"{"text":"x","calls":[]}"#.into());
        let b = JsonPayload(r#"{ "calls": [], "text": "x" }"#.into());
        assert_eq!(Exchange::from_payload(&a), Exchange::from_payload(&b));
    }

    #[test]
    fn damaged_checkpoints_are_rejected() {
        for bad in [
            "",
            "[]",
            r#"{"text":"x"}"#,
            r#"{"text":"x","calls":[{"call":"c","tool":"t","args":{}}]}"#,
            r#"{"text":"x","calls":[{"call":"c","tool":"t","args":{},"end":{"unknown":1}}]}"#,
            r#"{"text":1,"calls":[]}"#,
        ] {
            assert_eq!(
                Exchange::from_payload(&JsonPayload(bad.into())),
                None,
                "{bad}"
            );
        }
    }
}
