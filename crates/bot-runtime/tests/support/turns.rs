//! 가짜 상류(모델 제공자)의 SSE 응답 조립: 도구를 부르는 응답·마무리 글·다음 요청에 줄 응답 등록.
#![allow(dead_code)]

use bot_broker::testing::{End, FakeUpstream};

fn event(name: &str, data: &str) -> Vec<u8> {
    format!("event: {name}\ndata: {data}\n\n").into_bytes()
}

/// 모델이 도구 하나를 부르는 응답(입력 JSON은 한 조각으로).
pub fn tool_turn(message: &str, tool_id: &str, name: &str, input: &str) -> Vec<Vec<u8>> {
    vec![
        event(
            "message_start",
            &format!(
                r#"{{"type":"message_start","message":{{"id":"{message}","type":"message","role":"assistant","model":"model-large","content":[],"stop_reason":null,"usage":{{"input_tokens":5,"output_tokens":1}}}}}}"#
            ),
        ),
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

/// 모델이 한 응답에서 도구 여럿을 순서대로 부르는 응답: `(도구 사용 ID, 도구 이름, 입력 JSON)`들.
pub fn tools_turn(message: &str, calls: &[(&str, &str, &str)]) -> Vec<Vec<u8>> {
    let mut out = vec![event(
        "message_start",
        &format!(
            r#"{{"type":"message_start","message":{{"id":"{message}","type":"message","role":"assistant","model":"model-large","content":[],"stop_reason":null,"usage":{{"input_tokens":5,"output_tokens":1}}}}}}"#
        ),
    )];
    for (index, (tool_id, name, input)) in calls.iter().enumerate() {
        out.push(event(
            "content_block_start",
            &format!(r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"{tool_id}","name":"{name}","input":{{}}}}}}"#),
        ));
        out.push(event(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
                serde_json::to_string(input).unwrap()
            ),
        ));
        out.push(event(
            "content_block_stop",
            &format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
        ));
    }
    out.push(event(
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":4}}"#,
    ));
    out.push(event("message_stop", r#"{"type":"message_stop"}"#));
    out
}

/// 도구 결과를 받은 뒤의 마무리 글.
pub fn final_turn(message: &str, text: &str) -> Vec<Vec<u8>> {
    vec![
        event(
            "message_start",
            &format!(
                r#"{{"type":"message_start","message":{{"id":"{message}","type":"message","role":"assistant","model":"model-large","content":[],"stop_reason":null,"usage":{{"input_tokens":9,"output_tokens":1}}}}}}"#
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
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

/// 출력 토큰 상한(`max_tokens`)에서 끊긴 응답: 글 일부를 내다가 멈춘다. 실제 CLI는 이를 부분 글이 아니라 오류로 끝낸다.
pub fn cut_turn(message: &str, text: &str) -> Vec<Vec<u8>> {
    let mut chunks = final_turn(message, text);
    let delta = chunks.len() - 2;
    chunks[delta] = event(
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":4}}"#,
    );
    chunks
}

/// 공급자가 사용량을 보고하지 않은 응답: 시작 메시지와 끝 이벤트에서 `usage`를 뺀다(CLI는 이를 0으로 채워 낸다).
pub fn unreported(chunks: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
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
                    let mut event: serde_json::Value = serde_json::from_str(data).expect("JSON");
                    if let Some(message) = event.get_mut("message").and_then(|m| m.as_object_mut())
                    {
                        message.remove("usage");
                    }
                    if let Some(object) = event.as_object_mut() {
                        object.remove("usage");
                    }
                    format!("data: {event}")
                })
                .collect();
            (lines.join("\n") + "\n\n").into_bytes()
        })
        .collect()
}

/// 다음 모델 요청에 이 응답을 준다.
pub fn script(upstream: &FakeUpstream, chunks: Vec<Vec<u8>>) {
    let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
    upstream.respond_chunks(200, &refs, End::Eof);
}
