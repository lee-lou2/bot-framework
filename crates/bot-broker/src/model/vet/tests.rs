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
    // 문서: base64·본문 글·안에 블록을 담는 `content` 종류
    let docs = json!([
        {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": B64}},
        {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "notes"}},
        {"type": "document", "source": {"type": "content", "content": [{"type": "text", "text": "a"}, inline_image()]}},
    ]);
    assert!(check(user(docs)).is_ok());
    // 도구 결과는 글이거나 블록 목록이고, 그 안의 첨부도 같은 규칙을 받는다
    let result = json!([
        {"type": "tool_result", "tool_use_id": "t1", "content": "done"},
        {"type": "tool_result", "tool_use_id": "t2", "content": [{"type": "text", "text": "x"}, inline_image()]},
    ]);
    assert!(check(user(result)).is_ok());
    // 시스템 블록
    let body = json!({"system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}], "messages": []});
    assert!(check(body).is_ok());
}

#[test]
fn a_tool_use_input_is_data_and_a_search_result_source_is_only_a_label() {
    let blocks = json!([
        // 모델이 만든 도구 입력에 `source` 같은 이름이 있어도 첨부가 아니다
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
        // 도구 결과 안
        user(json!([{"type": "tool_result", "tool_use_id": "t", "content": [url.clone()]}])),
        // 도구 결과 안의 문서 안의 도구 결과 …
        user(
            json!([{"type": "tool_result", "tool_use_id": "t", "content": [
                {"type": "document", "source": {"type": "content", "content": [url.clone()]}}
            ]}]),
        ),
        // 객체 하나인 content(서버 도구 결과 모양)
        user(
            json!([{"type": "web_fetch_tool_result", "tool_use_id": "t", "content": url.clone()}]),
        ),
        // 메시지 content가 객체 하나
        user(url.clone()),
        // 시스템
        json!({"system": [url.clone()], "messages": []}),
        // 컨테이너 파일, 모양이 틀린 첨부, 모르는 종류, 깨진 content
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
    // 한도 안의 중첩은 받는다
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
    // 운영자가 연 다른 필드는 그대로 열린다
    let ok = json!({"messages": [], "x_extra": 1});
    assert!(vet_request(ok.as_object().unwrap(), &extra).is_ok());
}

#[test]
fn an_inline_attachment_needs_its_real_fields_and_types() {
    let bad = [
        // 종류 이름만 맞는 빈 껍데기
        json!({"type": "image", "source": {"type": "base64"}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png"}}),
        json!({"type": "image", "source": {"type": "base64", "data": B64}}),
        json!({"type": "document", "source": {"type": "text"}}),
        json!({"type": "document", "source": {"type": "content"}}),
        // 자료형이 틀리다
        json!({"type": "image", "source": {"type": "base64", "media_type": 5, "data": B64}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": 5}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": ["a"]}}),
        json!({"type": "document", "source": {"type": "content", "content": 5}}),
        // base64가 아닌 글(URL·공백·빈 값)
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "https://attacker.example/p.png"}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc def"}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": ""}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "===="}}),
        // 종류에 없는 필드를 끼웠다
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": B64, "url": "http://a.example/x"}}),
        json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "t", "file_id": "file_1"}}),
        json!({"type": "document", "source": {"type": "content", "content": [], "url": "http://a.example/x"}}),
        // 미디어 이름 모양이 아니다
        json!({"type": "image", "source": {"type": "base64", "media_type": "IMAGE/PNG", "data": B64}}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image", "data": B64}}),
        // 이미지는 base64만 받는다
        json!({"type": "image", "source": {"type": "text", "media_type": "text/plain", "data": "t"}}),
        json!({"type": "image", "source": {"type": "content", "content": []}}),
        // 블록에 모르는 필드가 있다
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": B64}, "url": "http://a.example/x"}),
        json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "t"}, "file_id": "file_1"}),
    ];
    for (i, block) in bad.iter().enumerate() {
        assert!(
            matches!(check(user(json!([block.clone()]))), Err(Error::Invalid)),
            "case {i}: {block}"
        );
    }
    // 갖춘 것은 통한다(문서 글 본문은 base64가 아니어도 된다)
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
