//! 완료한 모델 응답의 도구 목록 관측: 알리는 데 성공한 뒤에야 종결 이벤트·응답이 나간다.

use super::*;
use crate::front::observe::{CheckpointSink, Observed, ObservedUse, TurnSink};

fn event(name: &str, data: &str) -> Vec<u8> {
    format!("event: {name}\ndata: {data}\n\n").into_bytes()
}

fn start(id: &str) -> Vec<u8> {
    event(
        "message_start",
        &format!(
            r#"{{"type":"message_start","message":{{"id":"{id}","type":"message","role":"assistant","content":[],"model":"m","usage":{{"input_tokens":1,"output_tokens":1}}}}}}"#
        ),
    )
}

fn tool_block(index: u32, id: &str, name: &str, fragments: &[&str]) -> Vec<Vec<u8>> {
    let mut events = vec![event(
        "content_block_start",
        &format!(
            r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}}}"#
        ),
    )];
    for part in fragments {
        let part = serde_json::to_string(part).unwrap();
        events.push(event(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"input_json_delta","partial_json":{part}}}}}"#
            ),
        ));
    }
    events.push(event(
        "content_block_stop",
        &format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
    ));
    events
}

fn text_block(index: u32, text: &str) -> Vec<Vec<u8>> {
    vec![
        event(
            "content_block_start",
            &format!(
                r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"text","text":""}}}}"#
            ),
        ),
        event(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"text_delta","text":"{text}"}}}}"#
            ),
        ),
        event(
            "content_block_stop",
            &format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
        ),
    ]
}

fn tail() -> Vec<Vec<u8>> {
    vec![
        event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

async fn stream(r: &Rig, events: Vec<Vec<u8>>, end: End) -> Reply {
    let chunks: Vec<&[u8]> = events.iter().map(Vec::as_slice).collect();
    r.upstream.respond_chunks(200, &chunks, end);
    raw(
        r.front.addr(),
        &messages(r, r#"{"stream":true,"messages":[]}"#),
    )
    .await
}

fn used(id: &str, name: &str, input: &str) -> ObservedUse {
    ObservedUse {
        id: id.into(),
        name: name.into(),
        input: input.into(),
    }
}

#[tokio::test]
async fn a_completed_streamed_response_tells_its_tools_in_order_before_the_stop_event_goes_out() {
    let r = plain().await;
    let mut events = vec![start("msg_01ABC")];
    events.extend(text_block(0, "calling"));
    // 같은 도구·같은 인자 두 개(위치로 구별), 입력은 조각으로 쪼개져 온다
    events.extend(tool_block(
        1,
        "toolu_a",
        "echo_say",
        &["{\"te", "xt\":\"hi\"}"],
    ));
    events.extend(tool_block(2, "toolu_b", "echo_say", &["{\"text\":\"hi\"}"]));
    events.extend(tail());
    let reply = stream(&r, events, End::Eof).await;
    assert!(reply.body.contains("event: message_stop"), "{}", reply.body);
    assert_eq!(
        r.turns.seen(),
        [Observed {
            id: "msg_01ABC".into(),
            uses: vec![
                used("toolu_a", "echo_say", r#"{"text":"hi"}"#),
                used("toolu_b", "echo_say", r#"{"text":"hi"}"#),
            ],
        }]
    );
    assert_eq!(r.turns.attempts(), std::slice::from_ref(&r.attempt));
}

#[tokio::test]
async fn when_the_tools_cannot_be_told_the_stop_event_never_reaches_the_cli() {
    for (error, word) in [
        (bot_core::port::Error::Unavailable, "unavailable"),
        (bot_core::port::Error::Stale, "stale"),
    ] {
        let r = plain().await;
        r.turns.fail_with(Some(error));
        let mut events = vec![start("msg_01ABC")];
        events.extend(tool_block(0, "toolu_a", "echo_say", &["{}"]));
        events.extend(tail());
        let reply = stream(&r, events, End::Eof).await;
        assert!(
            !reply.body.contains("message_stop")
                && reply.body.contains(&format!("\"message\":\"{word}\"")),
            "{}",
            reply.body
        );
        assert!(r.turns.seen().is_empty());
    }
}

#[tokio::test]
async fn responses_without_tools_and_aborted_responses_tell_nothing() {
    let r = plain().await;
    let mut events = vec![start("msg_1")];
    events.extend(text_block(0, "just words"));
    events.extend(tail());
    let reply = stream(&r, events, End::Eof).await;
    assert!(reply.body.contains("message_stop"));
    assert!(r.turns.seen().is_empty());
    // 도구 블록이 다 오고도 종결 이벤트 없이 끊긴 응답
    let mut events = vec![start("msg_2")];
    events.extend(tool_block(0, "toolu_a", "echo_say", &["{}"]));
    let reply = stream(&r, events, End::Fail).await;
    assert!(!reply.body.contains("message_stop"), "{}", reply.body);
    assert!(r.turns.seen().is_empty());
}

#[tokio::test]
async fn an_input_that_is_not_a_json_object_is_never_told_or_released() {
    let r = plain().await;
    for fragments in [&["{\"text\":"][..], &["[1,2]"][..], &["not json"][..]] {
        let mut events = vec![start("msg_bad")];
        events.extend(tool_block(0, "toolu_a", "echo_say", fragments));
        events.extend(tail());
        let reply = stream(&r, events, End::Eof).await;
        assert!(
            !reply.body.contains("message_stop") && reply.body.contains("unavailable"),
            "{fragments:?}: {}",
            reply.body
        );
    }
    assert!(r.turns.seen().is_empty());
}

#[tokio::test]
async fn a_tool_with_no_input_and_a_start_event_that_carries_the_whole_input_are_read() {
    let r = plain().await;
    let mut events = vec![start("msg_x")];
    events.extend(tool_block(0, "toolu_a", "echo_say", &[]));
    events.push(event(
        "content_block_start",
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_b","name":"echo_say","input":{"text":"whole"}}}"#,
    ));
    events.push(event(
        "content_block_stop",
        r#"{"type":"content_block_stop","index":1}"#,
    ));
    events.extend(tail());
    stream(&r, events, End::Eof).await;
    assert_eq!(
        r.turns.seen()[0].uses,
        [
            used("toolu_a", "echo_say", "{}"),
            used("toolu_b", "echo_say", r#"{"text":"whole"}"#)
        ]
    );
}

#[tokio::test]
async fn a_plain_json_response_is_told_before_it_is_returned() {
    let r = plain().await;
    let body = r#"{"id":"msg_9","type":"message","role":"assistant","content":[{"type":"text","text":"x"},{"type":"tool_use","id":"toolu_n","name":"echo_say","input":{"text":"n"}}],"stop_reason":"tool_use"}"#;
    r.upstream.respond(200, body.as_bytes());
    let reply = raw(r.front.addr(), &messages(&r, r#"{"messages":[]}"#)).await;
    assert_eq!((reply.status, reply.body.as_str()), (200, body));
    assert_eq!(
        r.turns.seen(),
        [Observed {
            id: "msg_9".into(),
            uses: vec![used("toolu_n", "echo_say", r#"{"text":"n"}"#)],
        }]
    );
    // 알리지 못하면 응답 본문을 내보내지 않는다
    r.turns.fail_with(Some(bot_core::port::Error::Unavailable));
    r.upstream.respond(200, body.as_bytes());
    let reply = raw(r.front.addr(), &messages(&r, r#"{"messages":[]}"#)).await;
    assert_eq!(reply.status, 503);
    assert!(!reply.body.contains("toolu_n"), "{}", reply.body);
}

#[tokio::test]
async fn the_checkpoint_sink_writes_one_idempotent_step_per_response() {
    let runs = Arc::new(FakeRuns::new());
    let sink = CheckpointSink::new(runs.clone());
    let attempt = Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        generation: Generation(1),
        claim: Some(ClaimEpoch(1)),
    };
    let turn = Observed {
        id: "msg_01ABC".into(),
        uses: vec![used("toolu_a", "echo_say", r#"{"text":"hi"}"#)],
    };
    sink.completed(attempt.clone(), turn.clone()).await.unwrap();
    sink.completed(attempt.clone(), turn.clone()).await.unwrap();
    let steps = runs.steps();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].1.key, "cli-resp-msg_01ABC");
    let body: serde_json::Value = serde_json::from_str(&steps[0].1.body.0).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"id":"msg_01ABC","uses":[{"id":"toolu_a","name":"echo_say","input":{"text":"hi"}}]})
    );
    // 같은 응답 ID에 다른 내용은 받지 않는다
    let other = Observed {
        uses: vec![used("toolu_z", "echo_say", "{}")],
        ..turn
    };
    assert!(sink.completed(attempt.clone(), other).await.is_err());
    // 너무 큰 입력은 쓰지 않는다
    let big = Observed {
        id: "msg_big".into(),
        uses: vec![used(
            "toolu_a",
            "echo_say",
            &format!(r#"{{"text":"{}"}}"#, "a".repeat(1 << 20)),
        )],
    };
    assert!(sink.completed(attempt, big).await.is_err());
}

#[tokio::test]
async fn a_response_with_tools_but_no_usable_message_id_is_never_released() {
    let r = plain().await;
    // 메시지 ID가 없다 / 못 쓰는 모양이다
    for start_event in [
        event(
            "message_start",
            r#"{"type":"message_start","message":{"type":"message"}}"#,
        ),
        start("has space"),
        start(""),
    ] {
        let mut events = vec![start_event];
        events.extend(tool_block(0, "toolu_a", "echo_say", &["{}"]));
        events.extend(tail());
        let reply = stream(&r, events, End::Eof).await;
        assert!(
            !reply.body.contains("message_stop") && reply.body.contains("unavailable"),
            "{}",
            reply.body
        );
    }
    // 같은 응답 안에서 도구 사용 ID가 겹치거나 이름이 비었다
    for blocks in [
        [("toolu_a", "echo_say"), ("toolu_a", "echo_say")],
        [("toolu_a", "echo_say"), ("toolu_b", "")],
    ] {
        let mut events = vec![start("msg_1")];
        for (i, (id, name)) in blocks.iter().enumerate() {
            events.extend(tool_block(i as u32, id, name, &["{}"]));
        }
        events.extend(tail());
        let reply = stream(&r, events, End::Eof).await;
        assert!(!reply.body.contains("message_stop"), "{}", reply.body);
    }
    assert!(r.turns.seen().is_empty());
    // 도구가 없는 응답은 ID가 없어도 된다
    let mut events = vec![event(
        "message_start",
        r#"{"type":"message_start","message":{}}"#,
    )];
    events.extend(text_block(0, "words"));
    events.extend(tail());
    assert!(
        stream(&r, events, End::Eof)
            .await
            .body
            .contains("message_stop")
    );
    // 일반 JSON 응답도 같다
    r.upstream.respond(
        200,
        br#"{"content":[{"type":"tool_use","id":"toolu_n","name":"echo_say","input":{}}]}"#,
    );
    let reply = raw(r.front.addr(), &messages(&r, r#"{"messages":[]}"#)).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
}

mod ownership;
mod strict;
