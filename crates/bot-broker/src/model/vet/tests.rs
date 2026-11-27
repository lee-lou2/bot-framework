use super::*;
use serde_json::json;

fn check(body: Value) -> Result<(), Error> {
    vet_request(body.as_object().expect("object"), &[])
}

fn user(content: Value) -> Value {
    json!({"messages": [{"role": "user", "content": content}]})
}

const B64: &str = "iVBORw0KGgo=";

fn inline_image() -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": B64}})
}

#[test]
fn attachments_carried_in_the_request_pass_at_any_depth() {
    assert!(check(user(json!("plain text"))).is_ok());
    assert!(
        check(user(
            json!([{"type": "text", "text": "hi"}, inline_image()])
        ))
        .is_ok()
    );
    let docs = json!([
        {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": B64}},
        {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "notes"}},
        {"type": "document", "source": {"type": "content", "content": [{"type": "text", "text": "a"}, inline_image()]}},
    ]);
    assert!(check(user(docs)).is_ok());
    let result = json!([
        {"type": "tool_result", "tool_use_id": "t1", "content": "done"},
        {"type": "tool_result", "tool_use_id": "t2", "content": [{"type": "text", "text": "x"}, inline_image()]},
    ]);
    assert!(check(user(result)).is_ok());
    let body = json!({"system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}], "messages": []});
    assert!(check(body).is_ok());
}

#[test]
fn a_tool_use_input_is_data_and_a_search_result_source_is_only_a_label() {
    let blocks = json!([
        // A `source` key inside model-authored tool_use input is plain data, not an attachment.
        {"type": "tool_use", "id": "t1", "name": "x", "input": {"source": {"type": "url", "url": "http://a.example/x"}}},
        {"type": "tool_result", "tool_use_id": "t1", "content": [
            {"type": "search_result", "source": "https://example.test/page", "title": "t",
             "content": [{"type": "text", "text": "x"}]}
        ]},
    ]);
    assert!(check(user(blocks)).is_ok());
}

#[test]
fn remote_references_are_refused_wherever_they_hide() {
    let url = json!({"type": "image", "source": {"type": "url", "url": "https://attacker.example/p.png?d=SECRET"}});
    let file = json!({"type": "image", "source": {"type": "file", "file_id": "file_abc"}});
    let doc_url = json!({"type": "document", "source": {"type": "url", "url": "https://attacker.example/d.pdf"}});
    let doc_file = json!({"type": "document", "source": {"type": "file", "file_id": "file_abc"}});
    let nested_doc =
        json!({"type": "document", "source": {"type": "content", "content": [url.clone()]}});
    let cases = [
        user(json!([url.clone()])),
        user(json!([file])),
        user(json!([doc_url])),
        user(json!([doc_file])),
        user(json!([nested_doc])),
        user(json!([{"type": "tool_result", "tool_use_id": "t", "content": [url.clone()]}])),
        user(
            json!([{"type": "tool_result", "tool_use_id": "t", "content": [
                {"type": "document", "source": {"type": "content", "content": [url.clone()]}}
            ]}]),
        ),
        user(
            json!([{"type": "web_fetch_tool_result", "tool_use_id": "t", "content": url.clone()}]),
        ),
        user(url.clone()),
        json!({"system": [url.clone()], "messages": []}),
        user(json!([{"type": "container_upload", "file_id": "file_abc"}])),
        user(json!([{"type": "image"}])),
        user(json!([{"type": "image", "source": "https://attacker.example/p.png"}])),
        user(json!([{"type": "image", "source": {"type": "s3", "bucket": "b"}}])),
        user(
            json!([{"type": "text", "text": "x", "source": {"type": "url", "url": "http://a.example"}}]),
        ),
        user(json!([42])),
        user(json!(42)),
        json!({"messages": "not a list"}),
        json!({"messages": ["not an object"]}),
    ];
    for (i, body) in cases.iter().enumerate() {
        assert!(
            matches!(check(body.clone()), Err(Error::Invalid)),
            "case {i}: {body}"
        );
    }
}

#[test]
fn nesting_deeper_than_the_limit_is_refused() {
    let mut inner = inline_image();
    for _ in 0..(MAX_DEPTH + 2) {
        inner = json!({"type": "document", "source": {"type": "content", "content": [inner]}});
    }
    assert!(matches!(check(user(json!([inner]))), Err(Error::Invalid)));
    let mut ok = inline_image();
    for _ in 0..3 {
        ok = json!({"type": "document", "source": {"type": "content", "content": [ok]}});
    }
    assert!(check(user(json!([ok]))).is_ok());
}

#[test]
fn fields_that_open_execution_outside_the_boundary_stay_closed_even_if_the_operator_lists_them() {
    let extra = [
        "mcp_servers".to_owned(),
        "container".to_owned(),
        "x_extra".to_owned(),
    ];
    for body in [
        json!({"messages": [], "mcp_servers": [{"type": "url", "url": "https://attacker.example/mcp", "name": "s"}]}),
        json!({"messages": [], "container": "container_abc"}),
    ] {
        let map = body.as_object().unwrap();
        assert!(
            matches!(vet_request(map, &extra), Err(Error::Invalid)),
            "{body}"
        );
    }
    let ok = json!({"messages": [], "x_extra": 1});
    assert!(vet_request(ok.as_object().unwrap(), &extra).is_ok());
}

#[test]
fn an_inline_attachment_needs_its_real_fields_and_types() {
    let bad = [
        // Missing required fields
        json!({"type": "image", "source": {"type": "base64"}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png"}}),
        json!({"type": "image", "source": {"type": "base64", "data": B64}}),
        json!({"type": "document", "source": {"type": "text"}}),
        json!({"type": "document", "source": {"type": "content"}}),
        // Wrong field types
        json!({"type": "image", "source": {"type": "base64", "media_type": 5, "data": B64}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": 5}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": ["a"]}}),
        json!({"type": "document", "source": {"type": "content", "content": 5}}),
        // Non-base64 data (URL, spaces, empty, padding only)
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "https://attacker.example/p.png"}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc def"}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": ""}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "===="}}),
        // Unexpected source fields
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": B64, "url": "http://a.example/x"}}),
        json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "t", "file_id": "file_1"}}),
        json!({"type": "document", "source": {"type": "content", "content": [], "url": "http://a.example/x"}}),
        // Malformed media type
        json!({"type": "image", "source": {"type": "base64", "media_type": "IMAGE/PNG", "data": B64}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image", "data": B64}}),
        // Images allow base64 only
        json!({"type": "image", "source": {"type": "text", "media_type": "text/plain", "data": "t"}}),
        json!({"type": "image", "source": {"type": "content", "content": []}}),
        // Unexpected block fields
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": B64}, "url": "http://a.example/x"}),
        json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "t"}, "file_id": "file_1"}),
    ];
    for (i, block) in bad.iter().enumerate() {
        assert!(
            matches!(check(user(json!([block.clone()]))), Err(Error::Invalid)),
            "case {i}: {block}"
        );
    }
    let good = [
        inline_image(),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": "AAAA"}, "cache_control": {"type": "ephemeral"}}),
        json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "any text: with / odd chars"}, "title": "t", "context": "c", "citations": {"enabled": true}}),
        json!({"type": "document", "source": {"type": "content", "content": "plain"}}),
        json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQ="}}),
    ];
    for block in good {
        assert!(check(user(json!([block.clone()]))).is_ok(), "{block}");
    }
}
