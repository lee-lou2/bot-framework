//! Step-mode turn checkpoints (`turn-<n>` planned before execution, `end-<n>` completed exchange).

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TurnUsage {
    Reported(TokenUsage),
    Unreported,
    /// Legacy checkpoint missing the `usage` key; cannot be summed into a measured total.
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Planned {
    pub text: String,
    pub calls: Vec<PendingCall>,
    pub usage: TurnUsage,
    pub state: Option<String>,
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

#[derive(Debug, Default)]
pub(super) struct Restored {
    pub done: Vec<Exchange>,
    pub planned: Option<Planned>,
    pub usages: Vec<TurnUsage>,
}

/// Restores sequential completed turns and an optional in-flight plan; returns `None` on corruption.
pub(super) fn restore(steps: &[Step]) -> Option<Restored> {
    let by_key: HashMap<&str, &JsonPayload> =
        steps.iter().map(|s| (s.key.as_str(), &s.body)).collect();
    let mut restored = Restored::default();
    for turn in 0.. {
        let plan = by_key.get(plan_key(turn).as_str());
        if let Some(body) = by_key.get(end_key(turn).as_str()) {
            restored.done.push(Exchange::from_payload(body)?);
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
            text: "Looking it up".into(),
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
        assert_eq!(usage_of(""), TurnUsage::Unknown);
        assert_eq!(usage_of(r#","usage":null"#), TurnUsage::Unreported);
        assert_eq!(
            usage_of(r#","usage":{"prompt":3,"completion":2}"#),
            TurnUsage::Reported(TokenUsage {
                prompt: 3,
                completion: 2
            })
        );
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
            "non-string state must fail"
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
        assert_eq!(restored.usages, vec![planned().usage, planned().usage]);
        assert!(restore(&[]).unwrap().planned.is_none());
        assert!(restore(&[]).unwrap().usages.is_empty());
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
