use super::*;
use axum::http::StatusCode;

fn ok(json: &str) -> Chat {
    parse(json.as_bytes()).unwrap_or_else(|f| panic!("{json}: {} {:?}", f.code, f.param))
}

fn err(json: &str) -> (&'static str, Option<String>) {
    let failure = parse(json.as_bytes()).expect_err(json);
    assert_eq!(failure.status, StatusCode::BAD_REQUEST, "{json}");
    (failure.code, failure.param)
}

fn invalid(param: &str) -> (&'static str, Option<String>) {
    ("invalid_request", Some(param.to_owned()))
}

fn unsupported(param: &str) -> (&'static str, Option<String>) {
    ("unsupported_parameter", Some(param.to_owned()))
}

fn question(chat: &Chat) -> &str {
    let at = chat
        .body
        .current()
        .expect("validation guarantees a user turn");
    &chat.body.turns[at].content
}

const USER: &str = r#"{"role":"user","content":"hi"}"#;

fn with(extra: &str) -> String {
    format!(r#"{{"model":"assistant","messages":[{USER}],{extra}}}"#)
}

#[test]
fn the_smallest_request_is_a_model_and_one_user_message() {
    let chat = ok(&format!(r#"{{"model":"assistant","messages":[{USER}]}}"#));
    assert_eq!(chat.model, "assistant");
    assert!(!chat.stream && !chat.include_usage);
    assert_eq!(question(&chat), "hi");
    assert_eq!(chat.body.turns.len(), 1);
}

#[test]
fn all_turns_keep_their_order_roles_names_and_text() {
    let chat = ok(r#"{"model":"m","messages":[
            {"role":"system","content":"sys"},
            {"role":"developer","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]},
            {"role":"user","content":"first","name":"kim"},
            {"role":"assistant","content":"reply"},
            {"role":"user","content":"last question"},
            {"role":"assistant","content":"after"}]}"#);
    let turns: Vec<(&str, &str, Option<&str>)> = chat
        .body
        .turns
        .iter()
        .map(|t| (t.role.label(), t.content.as_str(), t.name.as_deref()))
        .collect();
    assert_eq!(
        turns,
        [
            ("system", "sys", None),
            ("developer", "a\nb", None),
            ("user", "first", Some("kim")),
            ("assistant", "reply", None),
            ("user", "last question", None),
            ("assistant", "after", None),
        ]
    );
    assert_eq!(question(&chat), "last question");
}

#[test]
fn stream_and_include_usage_are_read_and_include_usage_needs_stream() {
    let chat = ok(&with(
        r#""stream":true,"stream_options":{"include_usage":true}"#,
    ));
    assert!(chat.stream && chat.include_usage);
    let chat = ok(&with(r#""stream":true,"stream_options":{}"#));
    assert!(chat.stream && !chat.include_usage);
    let chat = ok(&with(r#""stream":false"#));
    assert!(!chat.stream);
    assert_eq!(
        err(&with(r#""stream_options":{"include_usage":true}"#)),
        invalid("stream_options")
    );
    assert_eq!(
        err(&with(
            r#""stream":false,"stream_options":{"include_usage":false}"#
        )),
        invalid("stream_options")
    );
    assert_eq!(
        err(&with(
            r#""stream":true,"stream_options":{"include_usage":1}"#
        )),
        invalid("stream_options.include_usage")
    );
    assert_eq!(
        err(&with(r#""stream":true,"stream_options":{"extra":true}"#)),
        unsupported("stream_options")
    );
    assert_eq!(err(&with(r#""stream":"yes""#)), invalid("stream"));
}

#[test]
fn syntax_shape_and_duplicates_are_invalid_requests() {
    for body in [
        "",
        "not json",
        "[]",
        "null",
        "\"model\"",
        r#"{"model":"a","model":"b","messages":[{"role":"user","content":"x"}]}"#,
        r#"{"model":"a","messages":[{"role":"user","content":"x","content":"y"}]}"#,
    ] {
        let failure = parse(body.as_bytes()).expect_err(body);
        assert_eq!(
            (failure.status, failure.code),
            (StatusCode::BAD_REQUEST, "invalid_request"),
            "{body}"
        );
        assert!(failure.param.is_none(), "{body}");
    }
}

#[test]
fn model_and_messages_are_required_and_typed() {
    assert_eq!(
        err(r#"{"messages":[{"role":"user","content":"x"}]}"#),
        invalid("model")
    );
    assert_eq!(
        err(r#"{"model":"","messages":[{"role":"user","content":"x"}]}"#),
        invalid("model")
    );
    assert_eq!(
        err(r#"{"model":3,"messages":[{"role":"user","content":"x"}]}"#),
        invalid("model")
    );
    assert_eq!(
        err(r#"{"model":null,"messages":[{"role":"user","content":"x"}]}"#),
        invalid("model")
    );
    assert_eq!(err(r#"{"model":"a"}"#), invalid("messages"));
    assert_eq!(err(r#"{"model":"a","messages":[]}"#), invalid("messages"));
    assert_eq!(err(r#"{"model":"a","messages":null}"#), invalid("messages"));
    assert_eq!(err(r#"{"model":"a","messages":"hi"}"#), invalid("messages"));
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"system","content":"x"}]}"#),
        invalid("messages")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":["x"]}"#),
        invalid("messages[0]")
    );
}

#[test]
fn a_blank_last_question_is_refused_but_a_blank_earlier_turn_is_data() {
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user","content":"  \n"}]}"#),
        invalid("messages[0].content")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user","content":""}]}"#),
        invalid("messages[0].content")
    );
    let chat = ok(
        r#"{"model":"a","messages":[{"role":"user","content":""},{"role":"user","content":"q"}]}"#,
    );
    assert_eq!(question(&chat), "q");
}

#[test]
fn message_roles_content_and_names_are_checked_with_paths() {
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"tool","content":"x"}]}"#),
        unsupported("messages[0].role")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"function","content":"x","name":"f"}]}"#),
        unsupported("messages[0].role")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"boss","content":"x"}]}"#),
        invalid("messages[0].role")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":7,"content":"x"}]}"#),
        invalid("messages[0].role")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"content":"x"}]}"#),
        invalid("messages[0].role")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user"}]}"#),
        invalid("messages[0].content")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user","content":null}]}"#),
        invalid("messages[0].content")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user","content":5}]}"#),
        invalid("messages[0].content")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user","content":[]}]}"#),
        invalid("messages[0].content")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user","content":"x","name":5}]}"#),
        invalid("messages[0].name")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user","content":"x","name":null}]}"#),
        invalid("messages[0].name")
    );
    assert_eq!(
        err(
            r#"{"model":"a","messages":[{"role":"assistant","content":"x","tool_calls":[]},{"role":"user","content":"q"}]}"#
        ),
        unsupported("messages[0].tool_calls")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user","content":"x","refusal":null}]}"#),
        unsupported("messages[0].refusal")
    );
}

#[test]
fn only_text_parts_are_supported_and_their_unknown_fields_are_checked() {
    let part =
        |p: &str| format!(r#"{{"model":"a","messages":[{{"role":"user","content":[{p}]}}]}}"#);
    ok(&part(r#"{"type":"text","text":"x"}"#));
    assert_eq!(
        err(&part(r#"{"type":"image_url","image_url":{"url":"u"}}"#)),
        unsupported("messages[0].content[0].type")
    );
    assert_eq!(
        err(&part(r#"{"type":"input_audio","input_audio":{}}"#)),
        unsupported("messages[0].content[0].type")
    );
    assert_eq!(
        err(&part(r#"{"type":"file","file":{}}"#)),
        unsupported("messages[0].content[0].type")
    );
    assert_eq!(
        err(&part(r#"{"type":"refusal","refusal":"x"}"#)),
        unsupported("messages[0].content[0].type")
    );
    assert_eq!(
        err(&part(r#"{"type":"text","text":"x","cache":true}"#)),
        unsupported("messages[0].content[0]")
    );
    assert_eq!(
        err(&part(r#"{"type":"text"}"#)),
        invalid("messages[0].content[0].text")
    );
    assert_eq!(
        err(&part(r#"{"type":"text","text":1}"#)),
        invalid("messages[0].content[0].text")
    );
    assert_eq!(
        err(&part(r#"{"text":"x"}"#)),
        invalid("messages[0].content[0].type")
    );
    assert_eq!(err(&part(r#""x""#)), invalid("messages[0].content[0]"));
    assert_eq!(
        err(&part(r#"{"type":"text","text":"x"},{"type":"image_url"}"#)),
        unsupported("messages[0].content[1].type")
    );
}

#[test]
fn nul_characters_are_refused_everywhere_text_goes() {
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user","content":"x\u0000"}]}"#),
        invalid("messages[0].content")
    );
    assert_eq!(
        err(r#"{"model":"a\u0000","messages":[{"role":"user","content":"x"}]}"#),
        invalid("model")
    );
    assert_eq!(
        err(r#"{"model":"a","messages":[{"role":"user","content":"x","name":"n\u0000"}]}"#),
        invalid("messages[0].name")
    );
}

#[test]
fn accepted_but_ignored_fields_are_range_checked_and_may_be_null() {
    ok(&with(
        r#""temperature":0,"top_p":1,"seed":-5,"user":"u-1","metadata":{"a":"b"},"store":false,"n":1,"response_format":{"type":"text"}"#,
    ));
    ok(&with(
        r#""temperature":null,"top_p":null,"seed":null,"metadata":null,"store":null,"n":null,"stream":null,"stream_options":null"#,
    ));
    assert_eq!(err(&with(r#""temperature":2.5"#)), invalid("temperature"));
    assert_eq!(err(&with(r#""temperature":-0.1"#)), invalid("temperature"));
    assert_eq!(err(&with(r#""temperature":"hot""#)), invalid("temperature"));
    assert_eq!(err(&with(r#""top_p":1.5"#)), invalid("top_p"));
    assert_eq!(err(&with(r#""seed":1.5"#)), invalid("seed"));
    assert_eq!(err(&with(r#""seed":"7""#)), invalid("seed"));
    assert_eq!(err(&with(r#""user":5"#)), invalid("user"));
    assert_eq!(err(&with(r#""user":null"#)), invalid("user"));
    assert_eq!(err(&with(r#""metadata":{"a":1}"#)), invalid("metadata"));
    assert_eq!(err(&with(r#""metadata":[1]"#)), invalid("metadata"));
    assert_eq!(err(&with(r#""store":true"#)), unsupported("store"));
    assert_eq!(err(&with(r#""store":"no""#)), invalid("store"));
    assert_eq!(err(&with(r#""n":2"#)), unsupported("n"));
    assert_eq!(err(&with(r#""n":0"#)), unsupported("n"));
    assert_eq!(err(&with(r#""n":1.5"#)), invalid("n"));
    assert_eq!(err(&with(r#""n":"1""#)), invalid("n"));
    assert_eq!(
        err(&with(r#""response_format":{"type":"json_object"}"#)),
        unsupported("response_format.type")
    );
    assert_eq!(
        err(&with(
            r#""response_format":{"type":"json_schema","json_schema":{}}"#
        )),
        unsupported("response_format.json_schema")
    );
    assert_eq!(
        err(&with(r#""response_format":"text""#)),
        invalid("response_format")
    );
    assert_eq!(
        err(&with(r#""response_format":null"#)),
        invalid("response_format")
    );
}

#[test]
fn what_the_server_cannot_keep_is_unsupported_and_null_means_omitted_only_where_nullable() {
    for field in [
        r#""max_tokens":10"#,
        r#""max_completion_tokens":10"#,
        r#""stop":"x""#,
        r#""stop":["a"]"#,
        r#""tools":[]"#,
        r#""tool_choice":"auto""#,
        r#""functions":[]"#,
        r#""function_call":"none""#,
        r#""parallel_tool_calls":true"#,
        r#""logprobs":true"#,
        r#""top_logprobs":2"#,
        r#""audio":{"voice":"x","format":"mp3"}"#,
        r#""modalities":["text"]"#,
        r#""frequency_penalty":0.5"#,
        r#""presence_penalty":0.5"#,
        r#""logit_bias":{"1":5}"#,
        r#""prediction":{"type":"content","content":"x"}"#,
        r#""reasoning_effort":"low""#,
        r#""service_tier":"auto""#,
        r#""web_search_options":{}"#,
        r#""prompt_cache_key":"k""#,
        r#""safety_identifier":"s""#,
        r#""verbosity":"low""#,
    ] {
        let (code, param) = err(&with(field));
        let name = field.split('"').nth(1).unwrap();
        assert_eq!(
            (code, param.as_deref()),
            ("unsupported_parameter", Some(name)),
            "{field}"
        );
    }
    for nullable in [
        "audio",
        "frequency_penalty",
        "logit_bias",
        "logprobs",
        "max_completion_tokens",
        "max_tokens",
        "modalities",
        "prediction",
        "presence_penalty",
        "reasoning_effort",
        "service_tier",
        "stop",
        "top_logprobs",
    ] {
        ok(&with(&format!(r#""{nullable}":null"#)));
    }
    for field in [
        "function_call",
        "functions",
        "parallel_tool_calls",
        "tool_choice",
        "tools",
        "web_search_options",
        "prompt_cache_key",
    ] {
        let (code, param) = err(&with(&format!(r#""{field}":null"#)));
        assert_eq!(
            (code, param.as_deref()),
            ("unsupported_parameter", Some(field)),
            "{field}"
        );
    }
}

#[test]
fn an_unknown_key_is_never_echoed_only_names_from_the_pinned_schema_are() {
    let secret = concat!("sk-live-", "0123456789", "abcdef");
    let at = |json: String| -> Failure { parse(json.as_bytes()).expect_err(&json) };
    let cases = [
        (
            format!(r#"{{"model":"a","messages":[{USER}],"{secret}":1}}"#),
            None,
        ),
        (
            format!(r#"{{"model":"a","messages":[{USER}],"{secret}":null}}"#),
            None,
        ),
        (
            format!(r#"{{"model":"a","messages":[{{"role":"user","content":"x","{secret}":1}}]}}"#),
            Some("messages[0]"),
        ),
        (
            format!(
                r#"{{"model":"a","messages":[{{"role":"user","content":[{{"type":"text","text":"x","{secret}":1}}]}}]}}"#
            ),
            Some("messages[0].content[0]"),
        ),
        (
            format!(
                r#"{{"model":"a","stream":true,"stream_options":{{"{secret}":1}},"messages":[{USER}]}}"#
            ),
            Some("stream_options"),
        ),
        (
            format!(r#"{{"model":"a","response_format":{{"{secret}":1}},"messages":[{USER}]}}"#),
            Some("response_format"),
        ),
    ];
    for (body, param) in cases {
        let failure = at(body);
        assert_eq!(failure.code, "unsupported_parameter");
        assert_eq!(failure.param.as_deref(), param);
        let rendered = failure.body().to_string();
        assert!(!rendered.contains(secret), "{rendered}");
        assert!(!rendered.contains("0123456789"), "{rendered}");
    }
    for name in TOP_FIELDS.iter().filter(|name| {
        ![
            "model",
            "messages",
            "stream",
            "stream_options",
            "n",
            "temperature",
            "top_p",
            "seed",
            "user",
            "metadata",
            "store",
            "response_format",
        ]
        .contains(name)
    }) {
        let failure =
            parse(format!(r#"{{"model":"a","messages":[{USER}],"{name}":1}}"#).as_bytes())
                .expect_err(name);
        assert_eq!(failure.param.as_deref(), Some(*name), "{name}");
    }
    let long = "k".repeat(65);
    for odd in [
        long.as_str(),
        "sp ace",
        "café",
        "a\"b",
        "a<b>",
        "tools2",
        "Tools",
        "messages ",
    ] {
        let body = format!(
            r#"{{"model":"a","messages":[{USER}],{}:1}}"#,
            serde_json::to_string(odd).unwrap()
        );
        let failure = parse(body.as_bytes()).expect_err(odd);
        assert_eq!(
            (failure.code, failure.param),
            ("unsupported_parameter", None),
            "{odd:?}"
        );
    }
    let failure = parse(
        br#"{"model":"a","messages":[{"role":"user","content":"SECRET-TEXT","extra-field":1}]}"#,
    )
    .expect_err("x");
    let rendered = format!("{:?}", failure.body());
    assert!(
        !rendered.contains("SECRET-TEXT") && !rendered.contains("extra-field"),
        "{rendered}"
    );
}

#[test]
fn an_unknown_top_level_field_is_refused_without_naming_it_even_when_it_is_null() {
    for value in ["1", "null", r#""x""#, "{}"] {
        assert_eq!(
            err(&with(&format!(r#""whatever":{value}"#))),
            ("unsupported_parameter", None),
            "{value}"
        );
    }
}
