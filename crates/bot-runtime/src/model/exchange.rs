//! Completed model turn and tool-call outcomes checkpointed by the worker across resumes.

use bot_core::{
    command::JsonPayload,
    id::{CallId, ToolId},
};
use serde_json::{Value, json};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallEnd {
    Done(JsonPayload),
    /// Fixed error vocabulary code (never raw error text).
    Failed(String),
    Denied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoneCall {
    pub call: CallId,
    pub tool: ToolId,
    pub args: JsonPayload,
    pub end: CallEnd,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Exchange {
    pub text: String,
    pub calls: Vec<DoneCall>,
    /// Opaque provider state (e.g. upstream call IDs or thought signatures) preserved verbatim across resumes.
    pub state: Option<String>,
}

fn json_of(payload: &JsonPayload) -> Value {
    serde_json::from_str(&payload.0).unwrap_or(Value::Null)
}

impl Exchange {
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
            text: "Checking now".into(),
            calls: vec![
                DoneCall {
                    call: CallId("r-1:0:0".into()),
                    tool: ToolId("agenda.list".into()),
                    args: JsonPayload("{}".into()),
                    end: CallEnd::Done(JsonPayload(
                        r#"{"events":[{"id":"e1","title":"Walk"}]}"#.into(),
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
        exchange.state = Some("{\"a\":\"line\\nbreak\"}".into());
        let back = Exchange::from_payload(&exchange.to_payload()).unwrap();
        assert_eq!(back.state, exchange.state);
        exchange.state = None;
        assert!(!exchange.to_payload().0.contains("state"));
        let old = JsonPayload(r#"{"text":"x","calls":[]}"#.into());
        assert_eq!(Exchange::from_payload(&old).unwrap().state, None);
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
