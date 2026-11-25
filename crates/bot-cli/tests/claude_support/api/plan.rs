//! 모델이 할 일(시험이 `POST /__ctl/script`로 정한다)을 상류(가짜)의 SSE 응답으로 바꾼다. 공식 Anthropic 스트림 모양이고 사용량은 시험이 정한 값 그대로다 —
//! 알면 `message_start.usage.input_tokens`와 `message_delta.usage.output_tokens`에, 모르면(공급자가 계수를 안 준 경우) 그 필드를 아예 싣지 않는다.
use crate::claude_support::tool_turn;
use bot_broker::testing::{End, FakeUpstream};
use serde_json::Value;
use std::time::Duration;

/// 모델 호출이 확정 거절(4xx)로 끝나게 하는 상류 응답 본문(CLI는 이런 오류를 다시 보내지 않는다)
pub const ERROR_BODY: &[u8] = br#"{"type":"error","error":{"type":"invalid_request_error","message":"e2e: the model refused"}}"#;

fn event(name: &str, data: &str) -> Vec<u8> {
    format!("event: {name}\ndata: {data}\n\n").into_bytes()
}

/// 글 하나를 내는 모델 차례. `usage`는 (입력, 출력) 토큰 또는 모름(None), `stop`은 `end_turn`(정상) 또는 `max_tokens`(실제 제한 종료).
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

/// 모델 차례에서 사용량 필드를 모두 걷어 낸다(공급자가 계수를 보고하지 않은 차례).
fn without_usage(chunks: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    chunks
        .into_iter()
        .map(|chunk| {
            let text = String::from_utf8(chunk).expect("SSE는 UTF-8");
            let lines: Vec<String> = text
                .lines()
                .map(|line| {
                    let Some(data) = line.strip_prefix("data: ") else {
                        return line.to_owned();
                    };
                    let mut event: Value = serde_json::from_str(data).expect("SSE data는 JSON");
                    if let Some(message) = event.get_mut("message").and_then(Value::as_object_mut) {
                        message.remove("usage");
                    }
                    event.as_object_mut().expect("객체").remove("usage");
                    format!("data: {event}")
                })
                .collect();
            (lines.join("\n") + "\n\n").into_bytes()
        })
        .collect()
}

/// 상류 대본에 쌓는다. 응답 전체가 `delay_ms`쯤 걸리도록 조각마다 같은 간격을 둔다. 쌓은 모델 응답 수를 돌려준다(남은 대본 수를 세려고).
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
        // 한 CLI 실행 안에서 도구를 한 번 부르고(첫 차례) 최종 답을 낸다(둘째 차례). 첫 차례의 계수는 기본 5/4이고 `first_usage: null`이면 보고하지 않는다.
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
        // 앞선 차례는 공급자 제한(max_tokens)으로 끝나고, CLI가 이어서 받은 다음 차례가 정상 최종 답이다
        send(text_turn(
            "msg_lim1",
            "앞 차례의 잘린 글",
            usage,
            "max_tokens",
        ));
        return 1 + push(upstream, &plan["then"]);
    }
    if plan["approval"] == true {
        // 모델이 승인이 필요한 도구를 부르고, 도구가 막힌 뒤에도 «성공했다»고 말하려 한다
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
