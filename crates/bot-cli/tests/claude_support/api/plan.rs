//! Translates scripted test plans (`POST /__ctl/script`) into Anthropic-format SSE responses for `FakeUpstream`.
use crate::claude_support::tool_turn;
use bot_broker::testing::{End, FakeUpstream};
use serde_json::Value;
use std::time::Duration;

/// 4xx refusal payload that the CLI treats as terminal without retrying.
pub const ERROR_BODY: &[u8] = br#"{"type":"error","error":{"type":"invalid_request_error","message":"e2e: the model refused"}}"#;

fn event(name: &str, data: &str) -> Vec<u8> {
    format!("event: {name}\ndata: {data}\n\n").into_bytes()
}

/// Single text turn; omits `usage` fields completely when `usage` is `None`.
pub fn text_turn(message: &str, text: &str, usage: Option<(u32, u32)>, stop: &str) -> Vec<Vec<u8>> {
    let start_usage = usage
        .map(|(input, _)| format!(r#","usage":{{"input_tokens":{input},"output_tokens":1}}"#))
        .unwrap_or_default();
    let delta_usage = usage
        .map(|(_, output)| format!(r#","usage":{{"output_tokens":{output}}}"#))
        .unwrap_or_default();
    vec![
        event(
            "message_start",
            &format!(
                r#"{{"type":"message_start","message":{{"id":"{message}","type":"message","role":"assistant","model":"model-large","content":[],"stop_reason":null{start_usage}}}}}"#
            ),
        ),
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
            &format!(
                r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop}","stop_sequence":null}}{delta_usage}}}"#
            ),
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

fn without_usage(chunks: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    chunks
        .into_iter()
        .map(|chunk| {
            let text = String::from_utf8(chunk).expect("SSE must be UTF-8");
            let lines: Vec<String> = text
                .lines()
                .map(|line| {
                    let Some(data) = line.strip_prefix("data: ") else {
                        return line.to_owned();
                    };
                    let mut event: Value =
                        serde_json::from_str(data).expect("SSE data must be JSON");
                    if let Some(message) = event.get_mut("message").and_then(Value::as_object_mut) {
                        message.remove("usage");
                    }
                    event.as_object_mut().expect("object").remove("usage");
                    format!("data: {event}")
                })
                .collect();
            (lines.join("\n") + "\n\n").into_bytes()
        })
        .collect()
}

/// Enqueues SSE responses on `upstream` and returns the number of upstream turns added.
pub fn push(upstream: &FakeUpstream, plan: &Value) -> usize {
    let text = plan["text"].as_str().unwrap_or("e2e-answer-ok");
    let delay = plan["delay_ms"].as_u64().unwrap_or(0);
    let usage = match plan.get("usage") {
        Some(Value::Null) => None,
        Some(Value::Array(pair)) => Some((
            pair[0].as_u64().unwrap_or(0) as u32,
            pair[1].as_u64().unwrap_or(0) as u32,
        )),
        _ => Some((9, 4)),
    };
    let stop = if plan["stop"] == "max_tokens" {
        "max_tokens"
    } else {
        "end_turn"
    };
    let send = |chunks: Vec<Vec<u8>>| {
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        let gap = Duration::from_millis(delay / refs.len().max(1) as u64);
        upstream.respond_slowly(200, &refs, gap, End::Eof);
    };
    if plan["fail"] == true {
        if delay > 0 {
            upstream.set_delay(Duration::from_millis(delay));
        }
        upstream.respond(400, ERROR_BODY);
        return 1;
    }
    if let Some(tool) = plan["tool_call"].as_str() {
        let first = tool_turn(
            "msg_tc1",
            "toolu_tc1",
            &format!("mcp__relay__agenda_{tool}"),
            "{}",
        );
        send(if plan.get("first_usage") == Some(&Value::Null) {
            without_usage(first)
        } else {
            first
        });
        send(text_turn("msg_tc2", text, usage, "end_turn"));
        return 2;
    }
    if plan["stop"] == "max_tokens" && plan.get("then").is_some() {
        send(text_turn(
            "msg_lim1",
            "Truncated text from previous turn",
            usage,
            "max_tokens",
        ));
        return 1 + push(upstream, &plan["then"]);
    }
    if plan["approval"] == true {
        // Calls an approval-gated tool, then attempts to claim success even though blocked.
        send(tool_turn(
            "msg_ap1",
            "toolu_ap1",
            "mcp__relay__agenda_remove",
            r#"{"id":"x"}"#,
        ));
        send(text_turn("msg_ap2", text, None, "end_turn"));
        return 2;
    }
    send(text_turn("msg_e2e", text, usage, stop));
    1
}
