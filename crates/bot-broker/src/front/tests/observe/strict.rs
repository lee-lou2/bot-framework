//! Strict observation tests rejecting any stream framing that could diverge from the CLI plan.

use super::*;

async fn send(r: &Rig, events: Vec<Vec<u8>>) -> String {
    stream(r, events, End::Eof).await.body
}

fn released(body: &str) -> bool {
    body.contains("message_stop")
}

fn raw_event(name: &str, data: &str) -> Vec<u8> {
    event(name, data)
}

fn block_start(index: u32, id: &str, input: &str) -> Vec<u8> {
    raw_event(
        "content_block_start",
        &format!(
            r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"{id}","name":"echo_say","input":{input}}}}}"#
        ),
    )
}

fn block_stop(index: u32) -> Vec<u8> {
    raw_event(
        "content_block_stop",
        &format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
    )
}

fn json_delta(index: u32, part: &str) -> Vec<u8> {
    let part = serde_json::to_string(part).unwrap();
    raw_event(
        "content_block_delta",
        &format!(
            r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"input_json_delta","partial_json":{part}}}}}"#
        ),
    )
}

#[tokio::test]
async fn a_start_event_input_that_is_not_an_object_is_never_stored_as_an_empty_object() {
    let r = plain().await;
    for input in ["[1,2]", "\"text\"", "7", "null", "true"] {
        let mut events = vec![
            start("msg_1"),
            block_start(0, "toolu_a", input),
            block_stop(0),
        ];
        events.extend(tail());
        let body = send(&r, events).await;
        assert!(
            !released(&body) && body.contains("unavailable"),
            "{input}: {body}"
        );
    }
    assert!(r.turns.seen().is_empty());
}

#[tokio::test]
async fn a_block_position_is_used_once_so_nothing_is_overwritten() {
    let r = plain().await;
    let cases: Vec<(&str, Vec<Vec<u8>>)> = vec![
        (
            "tool twice",
            vec![
                block_start(0, "toolu_a", "{}"),
                block_start(0, "toolu_b", "{}"),
                block_stop(0),
            ],
        ),
        (
            "restart after stop",
            vec![
                block_start(0, "toolu_a", "{}"),
                block_stop(0),
                block_start(0, "toolu_b", "{}"),
                block_stop(0),
            ],
        ),
        (
            "text then tool",
            [
                text_block(0, "x"),
                vec![block_start(0, "toolu_a", "{}"), block_stop(0)],
            ]
            .concat(),
        ),
        (
            "stop twice",
            vec![
                block_start(0, "toolu_a", "{}"),
                block_stop(0),
                block_stop(0),
            ],
        ),
        ("stop alone", vec![block_stop(3)]),
    ];
    for (label, blocks) in cases {
        let mut events = vec![start("msg_1")];
        events.extend(blocks);
        events.extend(tail());
        let body = send(&r, events).await;
        assert!(!released(&body), "{label}: {body}");
    }
    assert!(r.turns.seen().is_empty());
}

#[tokio::test]
async fn deltas_outside_an_open_tool_block_and_unfinished_blocks_are_refused() {
    let r = plain().await;
    let cases: Vec<(&str, Vec<Vec<u8>>)> = vec![
        (
            "delta before start",
            vec![
                json_delta(0, "{}"),
                block_start(0, "toolu_a", "{}"),
                block_stop(0),
            ],
        ),
        (
            "delta after stop",
            vec![
                block_start(0, "toolu_a", "{}"),
                block_stop(0),
                json_delta(0, "{}"),
            ],
        ),
        (
            "input fragment on a text block",
            [
                vec![raw_event(
                    "content_block_start",
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                )],
                vec![json_delta(0, "{}")],
                vec![block_stop(0)],
            ]
            .concat(),
        ),
        (
            "never stopped",
            vec![block_start(0, "toolu_a", "{}"), json_delta(0, "{}")],
        ),
        (
            "text never stopped",
            vec![raw_event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            )],
        ),
        (
            "both a start input and fragments",
            vec![
                block_start(0, "toolu_a", r#"{"a":1}"#),
                json_delta(0, r#"{"a":2}"#),
                block_stop(0),
            ],
        ),
        (
            "fragment that is not an object",
            vec![
                block_start(0, "toolu_a", "{}"),
                json_delta(0, "[1]"),
                block_stop(0),
            ],
        ),
    ];
    for (label, blocks) in cases {
        let mut events = vec![start("msg_1")];
        events.extend(blocks);
        events.extend(tail());
        let body = send(&r, events).await;
        assert!(!released(&body), "{label}: {body}");
    }
    assert!(r.turns.seen().is_empty());
}

#[tokio::test]
async fn duplicate_keys_names_that_disagree_and_hidden_events_are_refused() {
    let r = plain().await;
    let cases: Vec<(&str, Vec<Vec<u8>>)> = vec![
        (
            "duplicate key in an event",
            vec![
                block_start(0, "toolu_a", "{}"),
                raw_event(
                    "content_block_stop",
                    r#"{"type":"content_block_stop","index":0,"index":1}"#,
                ),
            ],
        ),
        (
            "duplicate key in the tool input",
            vec![
                block_start(0, "toolu_a", "{}"),
                json_delta(0, r#"{"a":1,"a":2}"#),
                block_stop(0),
            ],
        ),
        (
            "ping hiding a block start",
            vec![
                raw_event(
                    "ping",
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_h","name":"echo_say","input":{}}}"#,
                ),
                raw_event(
                    "content_block_stop",
                    r#"{"type":"content_block_stop","index":0}"#,
                ),
            ],
        ),
        (
            "block start named as a ping",
            vec![raw_event(
                "content_block_start",
                r#"{"type":"ping","index":0,"content_block":{"type":"tool_use","id":"toolu_h","name":"echo_say","input":{}}}"#,
            )],
        ),
        (
            "block start without a name",
            vec![b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_h\",\"name\":\"echo_say\",\"input\":{}}}\n\n".to_vec()],
        ),
        (
            "two data lines",
            vec![b"event: ping\ndata: {}\ndata: {}\n\n".to_vec()],
        ),
        (
            "data that is not json",
            vec![b"event: ping\ndata: not json\n\n".to_vec()],
        ),
        ("second message start", vec![start("msg_2")]),
    ];
    for (label, blocks) in cases {
        let mut events = vec![start("msg_1")];
        events.extend(blocks);
        events.extend(tail());
        let body = send(&r, events).await;
        assert!(!released(&body), "{label}: {body}");
    }
    assert!(r.turns.seen().is_empty());
}

#[tokio::test]
async fn a_well_formed_response_with_every_block_kind_is_still_released() {
    let r = plain().await;
    let thinking = vec![
        raw_event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        ),
        raw_event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
        ),
        block_stop(0),
    ];
    let mut events = vec![start("msg_ok")];
    events.extend(thinking);
    events.push(raw_event("ping", r#"{"type":"ping"}"#));
    events.extend([
        block_start(1, "toolu_a", "{}"),
        json_delta(1, r#"{"te"#),
        json_delta(1, r#"xt":"hi"}"#),
        block_stop(1),
        block_start(2, "toolu_b", r#"{"text":"whole"}"#),
        block_stop(2),
    ]);
    events.extend(tail());
    let body = send(&r, events).await;
    assert!(released(&body), "{body}");
    assert_eq!(
        r.turns.seen()[0].uses,
        [
            used("toolu_a", "echo_say", r#"{"text":"hi"}"#),
            used("toolu_b", "echo_say", r#"{"text":"whole"}"#),
        ]
    );
}

#[tokio::test]
async fn a_final_event_that_looks_like_a_stop_but_is_not_one_is_never_written() {
    let r = plain().await;
    for last in [
        raw_event("ping", r#"{"type":"message_stop"}"#),
        b"data: {\"type\":\"message_stop\"}\n\n".to_vec(),
        raw_event("message_stop", r#"{"type":"ping"}"#),
    ] {
        let mut events = vec![start("msg_1")];
        events.extend(tool_block(0, "toolu_a", "echo_say", &["{}"]));
        events.push(last.clone());
        let body = send(&r, events).await;
        assert!(
            !body.contains("message_stop") && body.contains("\"message\":\"unavailable\""),
            "{}: {body}",
            String::from_utf8_lossy(&last)
        );
        assert!(r.turns.seen().is_empty());
    }
}
