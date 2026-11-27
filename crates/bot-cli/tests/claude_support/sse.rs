//! FakeUpstream SSE response builders and request inspection helpers.
#![allow(dead_code)]

use bot_broker::testing::FakeUpstream;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

fn event(name: &str, data: &str) -> Vec<u8> {
    format!("event: {name}\ndata: {data}\n\n").into_bytes()
}

fn message_start(message: &str, input_tokens: u32) -> Vec<u8> {
    event(
        "message_start",
        &format!(
            r#"{{"type":"message_start","message":{{"id":"{message}","type":"message","role":"assistant","model":"model-large","content":[],"stop_reason":null,"usage":{{"input_tokens":{input_tokens},"output_tokens":1}}}}}}"#
        ),
    )
}

pub fn tool_turn(message: &str, tool_id: &str, name: &str, input: &str) -> Vec<Vec<u8>> {
    vec![
        message_start(message, 5),
        event(
            "content_block_start",
            &format!(
                r#"{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"{tool_id}","name":"{name}","input":{{}}}}}}"#
            ),
        ),
        event(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
                serde_json::to_string(input).unwrap()
            ),
        ),
        event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":4}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

pub fn final_turn(message: &str, text: &str) -> Vec<Vec<u8>> {
    vec![
        message_start(message, 9),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        event(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":{}}}}}"#,
                serde_json::to_string(text).unwrap()
            ),
        ),
        event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

pub fn bodies(upstream: &FakeUpstream) -> Vec<Value> {
    upstream
        .requests()
        .into_iter()
        .map(|seen| serde_json::from_str(&seen.3).expect("request body must be JSON"))
        .collect()
}

pub fn result_text(body: &Value, tool_use_id: &str) -> Option<String> {
    for message in body["messages"].as_array()? {
        let Some(blocks) = message["content"].as_array() else {
            continue;
        };
        for block in blocks {
            if block["type"] == "tool_result" && block["tool_use_id"] == tool_use_id {
                return Some(match &block["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    other => other.to_string(),
                });
            }
        }
    }
    None
}

pub fn tool_result(upstream: &FakeUpstream, tool_use_id: &str) -> Option<String> {
    bodies(upstream)
        .iter()
        .find_map(|body| result_text(body, tool_use_id))
}

pub fn id_in(result: &str) -> String {
    serde_json::from_str::<Value>(result)
        .ok()
        .and_then(|value| value["schedule"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("tool result missing schedule field: {result}"))
}

pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub fn in_secs(after: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    rfc3339(now + after)
}
