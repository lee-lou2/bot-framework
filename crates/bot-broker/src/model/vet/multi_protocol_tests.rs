use super::*;
use serde_json::json;

const B64: &str = "iVBORw0KGgo=";

fn vet_responses(body: Value) -> Result<(), Error> {
    vet_request(
        Protocol::OpenAiResponses,
        body.as_object().expect("object"),
        &[],
    )
}

fn vet_gemini(body: Value) -> Result<(), Error> {
    vet_request(Protocol::Gemini, body.as_object().expect("object"), &[])
}

#[test]
fn responses_accepts_codex_wire_shape_and_inline_data_images() {
    let body = json!({
        "model": "gpt-5",
        "stream": true,
        "store": false,
        "instructions": "follow rules",
        "parallel_tool_calls": true,
        "prompt_cache_key": "k-1",
        "reasoning": {"effort": "medium"},
        "tool_choice": "auto",
        "include": ["reasoning.encrypted_content"],
        "client_metadata": {"cli": "codex"},
        "tools": [
            {
                "type": "namespace",
                "name": "mcp__relay",
                "tools": [
                    {"type": "function", "name": "echo_say", "parameters": {"type": "object"}}
                ]
            },
            {"type": "function", "name": "request_user_input", "parameters": {"type": "object"}}
        ],
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "hello"},
                    {"type": "input_image", "image_url": format!("data:image/png;base64,{B64}")}
                ]
            },
            {"type": "function_call", "call_id": "call_1", "name": "echo_say", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ]
    });
    assert!(vet_responses(body).is_ok());
}

#[test]
fn responses_refuses_server_tools_storage_and_remote_references() {
    let cases = [
        json!({"store": true, "input": []}),
        json!({"store": "false", "input": []}),
        json!({"background": true, "input": []}),
        json!({"previous_response_id": "resp_1", "input": []}),
        json!({"mcp_servers": [], "input": []}),
        json!({"container": "c1", "input": []}),
        json!({"unknown_field": 1, "input": []}),
        json!({"tools": [{"type": "web_search"}], "input": []}),
        json!({"tools": [{"type": "web_search_preview"}], "input": []}),
        json!({"tools": [{"type": "file_search"}], "input": []}),
        json!({"tools": [{"type": "code_interpreter"}], "input": []}),
        json!({"tools": [{"type": "computer_use_preview"}], "input": []}),
        json!({"tools": [{"type": "mcp", "server_url": "https://evil.example"}], "input": []}),
        json!({"tools": [{"type": "local_shell"}], "input": []}),
        json!({"tools": [{"type": "shell"}], "input": []}),
        json!({"tools": [{"type": "apply_patch"}], "input": []}),
        json!({"tools": [{"type": "image_generation"}], "input": []}),
        json!({"tools": [{"type": "namespace", "name": "mcp__relay", "tools": [{"type": "web_search"}]}], "input": []}),
        json!({"input": [{"type": "message", "role": "user", "content": [
            {"type": "input_image", "image_url": "https://attacker.example/leak.png"}
        ]}]}),
        json!({"input": [{"type": "message", "role": "user", "content": [
            {"type": "input_image", "file_id": "file_123"}
        ]}]}),
        json!({"input": [{"type": "message", "role": "user", "content": [
            {"type": "input_file", "file_url": "https://attacker.example/doc.pdf"}
        ]}]}),
        json!({"input": [{"type": "message", "role": "user", "content": [
            {"type": "input_image", "image_url": "data:image/png;base64,not valid base64!"}
        ]}]}),
        json!({"input": [{"type": "message", "role": "user", "content": [
            {"type": "input_image", "image_url": "data:IMAGE/PNG;base64,AAAA"}
        ]}]}),
        // H2 regressions: object function_call_output, computer_call_output, item-level image_url/input_image.
        json!({"input": [{"type": "function_call_output", "call_id": "c1", "output": {"type": "input_image", "image_url": "https://evil.example/ssrf"}}]}),
        json!({"input": [{"type": "computer_call_output", "call_id": "c1", "output": {"type": "computer_screenshot", "image_url": "https://evil.example/ssrf"}}]}),
        json!({"input": [{"type": "input_image", "image_url": "https://evil.example/ssrf"}]}),
        json!({"input": [{"type": "message", "role": "user", "image_url": "https://evil.example/ssrf"}]}),
    ];
    for (i, body) in cases.iter().enumerate() {
        assert!(
            matches!(vet_responses(body.clone()), Err(Error::Invalid)),
            "case {i}: {body}"
        );
    }
    let extra = [
        "background".to_owned(),
        "previous_response_id".to_owned(),
        "x_custom".to_owned(),
    ];
    for reserved in [
        json!({"background": true, "input": []}),
        json!({"previous_response_id": "resp_1", "input": []}),
    ] {
        assert!(matches!(
            vet_request(
                Protocol::OpenAiResponses,
                reserved.as_object().unwrap(),
                &extra
            ),
            Err(Error::Invalid)
        ));
    }
    let ok_extra = json!({"x_custom": 1, "input": []});
    assert!(
        vet_request(
            Protocol::OpenAiResponses,
            ok_extra.as_object().unwrap(),
            &extra
        )
        .is_ok()
    );
}

#[test]
fn gemini_accepts_agy_wire_shape_and_valid_inline_data() {
    let body = json!({
        "systemInstruction": {"parts": [{"text": "rules"}]},
        "generationConfig": {"maxOutputTokens": 4096},
        "tools": [
            {"functionDeclarations": [{"name": "call_mcp_tool", "parametersJsonSchema": {"type": "object"}}]}
        ],
        "contents": [
            {
                "role": "user",
                "parts": [
                    {"text": "hi"},
                    {"inlineData": {"mimeType": "image/png", "data": B64}}
                ]
            },
            {
                "role": "model",
                "parts": [
                    {"thought": true, "text": "plan"},
                    {"functionCall": {"name": "call_mcp_tool", "args": {"ServerName": "relay", "ToolName": "echo_say", "Arguments": {}}}}
                ]
            }
        ]
    });
    assert!(vet_gemini(body).is_ok());
}

#[test]
fn gemini_refuses_server_tools_cached_content_and_remote_file_parts() {
    let cases = [
        json!({"cachedContent": "cachedContents/1", "contents": []}),
        json!({"model": "gemini-2.5-pro", "contents": []}),
        json!({"tools": [{"googleSearch": {}}], "contents": []}),
        json!({"tools": [{"googleSearchRetrieval": {}}], "contents": []}),
        json!({"tools": [{"codeExecution": {}}], "contents": []}),
        json!({"tools": [{"urlContext": {}}], "contents": []}),
        json!({"tools": [{"enterpriseWebSearch": {}}], "contents": []}),
        json!({"tools": [{"googleMaps": {}}], "contents": []}),
        json!({"tools": [{}], "contents": []}),
        json!({"tools": [{"functionDeclarations": [], "googleSearch": {}}], "contents": []}),
        json!({"contents": [{"role": "user", "parts": [{"fileData": {"fileUri": "gs://bucket/x", "mimeType": "text/plain"}}]}]}),
        json!({"systemInstruction": {"parts": [{"fileData": {"fileUri": "https://attacker.example/a"}}]}, "contents": []}),
        json!({"contents": [{"role": "model", "parts": [{"executableCode": {"code": "print(1)"}}]}]}),
        json!({"contents": [{"role": "user", "parts": [{"codeExecutionResult": {"output": "1"}}]}]}),
        json!({"contents": [{"role": "user", "parts": [{"inlineData": {"mimeType": "IMAGE/PNG", "data": B64}}]}]}),
        json!({"contents": [{"role": "user", "parts": [{"inlineData": {"mimeType": "image/png", "data": "https://attacker.example/x"}}]}]}),
        json!({"contents": [{"role": "user", "parts": [{"inlineData": {"mimeType": "image/png", "data": B64, "fileUri": "x"}}]}]}),
        // H1 regressions: ProtoJSON snake_case bypasses and unvetted toolConfig.
        json!({"cached_content": "cachedContents/1", "contents": []}),
        json!({"contents": [{"role": "user", "parts": [{"file_data": {"mime_type": "text/plain", "file_uri": "https://evil.example/a"}}]}]}),
        json!({"contents": [{"role": "model", "parts": [{"executable_code": {"code": "print(1)"}}]}]}),
        json!({"contents": [{"role": "user", "parts": [{"code_execution_result": {"output": "1"}}]}]}),
        json!({"contents": [{"role": "user", "parts": [{"function_response": {"name": "x", "parts": [{"file_data": {"file_uri": "https://evil.example"}}]}}]}]}),
        json!({"toolConfig": {"retrievalConfig": {"latLng": {"latitude": 0, "longitude": 0}}}, "contents": []}),
    ];
    for (i, body) in cases.iter().enumerate() {
        assert!(
            matches!(vet_gemini(body.clone()), Err(Error::Invalid)),
            "case {i}: {body}"
        );
    }
    assert!(vet_gemini(json!({"toolConfig": {"functionCallingConfig": {"mode": "AUTO", "allowedFunctionNames": ["call_mcp_tool"]}}, "contents": []})).is_ok());
    let extra = [
        "cachedContent".to_owned(),
        "cached_content".to_owned(),
        "x_extra".to_owned(),
    ];
    for reserved in [
        json!({"cachedContent": "cachedContents/1", "contents": []}),
        json!({"cached_content": "cachedContents/1", "contents": []}),
    ] {
        assert!(matches!(
            vet_request(Protocol::Gemini, reserved.as_object().unwrap(), &extra),
            Err(Error::Invalid)
        ));
    }
    let ok_extra = json!({"x_extra": true, "contents": []});
    assert!(vet_request(Protocol::Gemini, ok_extra.as_object().unwrap(), &extra).is_ok());
}
