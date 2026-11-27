//! Strict upstream response parsing tests: unambiguous object args, unique call IDs, stop reasons, and provider-bound state replay.

use super::{done_call, exchange, request, settings, tool, user};
use crate::model::api::{ApiEndpoint, ApiKind, wire};
use crate::model::{CallEnd, ModelError, ModelRequest, ModelStep};
use bot_core::{
    command::JsonPayload,
    id::{SecretName, ToolId},
};

fn failed(code: &'static str, transient: bool) -> ModelError {
    wire::fail(code, transient)
}

fn base() -> ModelRequest {
    request(
        vec![user("Show today's agenda")],
        vec![tool(
            "agenda.list",
            "List agenda events",
            r#"{"type":"object"}"#,
        )],
        vec![],
    )
}

fn alias(kind: ApiKind) -> String {
    let built = kind.build(&settings(kind), &base()).unwrap();
    built
        .names
        .alias(&ToolId("agenda.list".into()))
        .unwrap()
        .to_owned()
}

fn read(kind: ApiKind, raw: &str) -> Result<ModelStep, ModelError> {
    let req = base();
    let built = kind.build(&settings(kind), &req).unwrap();
    kind.parse(&settings(kind), raw.as_bytes(), &req, &built.names)
}

fn openai(tool_calls: &str) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"content":null,"tool_calls":[{tool_calls}]}},"finish_reason":"tool_calls"}}],"usage":{{"prompt_tokens":5,"completion_tokens":2}}}}"#
    )
}

fn anthropic(stop: &str, blocks: &str) -> String {
    format!(
        r#"{{"content":[{blocks}],"stop_reason":"{stop}","usage":{{"input_tokens":5,"output_tokens":2}}}}"#
    )
}

fn call_count(step: Result<ModelStep, ModelError>) -> usize {
    match step {
        Ok(ModelStep::Calls { calls, .. }) => calls.len(),
        other => panic!("expected tool calls: {other:?}"),
    }
}

#[test]
fn a_tool_call_without_usable_arguments_runs_nothing() {
    let name = alias(ApiKind::OpenAi);
    let none = failed("invalid_tool_args", false);
    for arguments in [
        r#""arguments":"","#,
        r#""arguments":"   ","#,
        r#""arguments":null,"#,
        r#""arguments":"[1]","#,
        r#""arguments":"{\"event\":\"a\",\"event\":\"b\"}","#,
        r#""arguments":"{\"o\":{\"k\":1,\"k\":2}}","#,
        r#""arguments":{"event":"a","event":"b"},"#,
        "",
    ] {
        let raw = openai(&format!(
            r#"{{"id":"c1","type":"function","function":{{{arguments}"name":"{name}"}}}}"#
        ));
        assert_eq!(
            read(ApiKind::OpenAi, &raw).unwrap_err(),
            none,
            "openai {arguments}"
        );
    }
    let name = alias(ApiKind::Anthropic);
    for input in [
        "",
        r#","input":null"#,
        r#","input":"string""#,
        r#","input":[1]"#,
        r#","input":{"event":"a","event":"b"}"#,
        r#","input":{"o":{"k":1,"k":2}}"#,
    ] {
        let raw = anthropic(
            "tool_use",
            &format!(r#"{{"type":"tool_use","id":"a","name":"{name}"{input}}}"#),
        );
        assert_eq!(
            read(ApiKind::Anthropic, &raw).unwrap_err(),
            none,
            "anthropic {input}"
        );
    }
    let raw = openai(&format!(
        r#"{{"id":"c1","type":"function","function":{{"name":"{name}","arguments":"{{}}"}}}}"#
    ));
    assert_eq!(call_count(read(ApiKind::OpenAi, &raw)), 1);
    let raw = anthropic(
        "tool_use",
        &format!(r#"{{"type":"tool_use","id":"a","name":"{name}","input":{{}}}}"#),
    );
    assert_eq!(call_count(read(ApiKind::Anthropic, &raw)), 1);
}

#[test]
fn the_same_upstream_call_id_twice_in_one_response_runs_nothing() {
    let name = alias(ApiKind::OpenAi);
    let item = |id: &str| {
        format!(
            r#"{{"id":"{id}","type":"function","function":{{"name":"{name}","arguments":"{{}}"}}}}"#
        )
    };
    let twice = openai(&format!("{},{}", item("dup"), item("dup")));
    assert_eq!(
        read(ApiKind::OpenAi, &twice).unwrap_err(),
        failed("invalid_response", false)
    );
    let spaced = openai(&format!("{},{}", item("a b"), item("a b")));
    assert_eq!(
        read(ApiKind::OpenAi, &spaced).unwrap_err(),
        failed("invalid_response", false)
    );
    let distinct = openai(&format!("{},{}", item("one"), item("two")));
    assert_eq!(call_count(read(ApiKind::OpenAi, &distinct)), 2);

    let name = alias(ApiKind::Anthropic);
    let block =
        |id: &str| format!(r#"{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}"#);
    let twice = anthropic("tool_use", &format!("{},{}", block("dup"), block("dup")));
    assert_eq!(
        read(ApiKind::Anthropic, &twice).unwrap_err(),
        failed("invalid_response", false)
    );
    let distinct = anthropic("tool_use", &format!("{},{}", block("one"), block("two")));
    assert_eq!(call_count(read(ApiKind::Anthropic, &distinct)), 2);
}

#[test]
fn an_anthropic_answer_that_stopped_for_another_reason_cannot_carry_a_tool_use() {
    let name = alias(ApiKind::Anthropic);
    let block = format!(r#"{{"type":"tool_use","id":"a","name":"{name}","input":{{}}}}"#);
    for stop in ["end_turn", "stop_sequence"] {
        assert_eq!(
            read(ApiKind::Anthropic, &anthropic(stop, &block)).unwrap_err(),
            failed("invalid_response", false),
            "{stop}"
        );
    }
    assert_eq!(
        call_count(read(ApiKind::Anthropic, &anthropic("tool_use", &block))),
        1
    );
    let text = anthropic("end_turn", r#"{"type":"text","text":"done"}"#);
    assert!(matches!(
        read(ApiKind::Anthropic, &text),
        Ok(ModelStep::Final { .. })
    ));
}

#[test]
fn the_providers_ids_and_signatures_are_resent_only_to_the_provider_that_issued_them() {
    let kind = ApiKind::OpenAi;
    let name = alias(kind);
    let issued = openai(&format!(
        r#"{{"id":"call_up_1","type":"function","function":{{"name":"{name}","arguments":"{{}}"}},"extra_content":{{"google":{{"thought_signature":"SIG-ONE"}}}}}}"#
    ));
    let Ok(ModelStep::Calls {
        state: Some(state), ..
    }) = read(kind, &issued)
    else {
        panic!("expected upstream provider state");
    };
    let next = |to: &crate::model::api::ApiSettings| -> String {
        let past = exchange(
            "",
            vec![done_call(
                "c1",
                "agenda.list",
                "{}",
                CallEnd::Done(JsonPayload("{}".into())),
            )],
            Some(state.as_str()),
        );
        let req = request(vec![user("continue")], base().tools, vec![past]);
        String::from_utf8(kind.build(to, &req).unwrap().body).unwrap()
    };
    let same = next(&settings(kind));
    assert!(
        same.contains("call_up_1") && same.contains("SIG-ONE"),
        "{same}"
    );
    let mut other_model = settings(kind);
    other_model.model = "model-y".into();
    let mut other_endpoint = settings(kind);
    other_endpoint.endpoint = ApiEndpoint::parse("https://other.example.com/v1").unwrap();
    let mut other_secret = settings(kind);
    other_secret.secret = SecretName::parse("other-key");
    for (what, changed) in [
        ("model", other_model),
        ("endpoint", other_endpoint),
        ("secret", other_secret),
    ] {
        let body = next(&changed);
        assert!(
            !body.contains("call_up_1") && !body.contains("SIG-ONE"),
            "old provider ID/signature leaked after {what} changed: {body}"
        );
        assert!(
            body.contains("tool_call_id"),
            "{what}: tool results must still link to calls"
        );
    }
}

#[test]
fn the_output_limit_goes_out_under_the_field_name_the_operator_wrote() {
    use crate::model::api::LimitField;
    let body = |kind: ApiKind, limit_field: LimitField| {
        let mut with = settings(kind);
        with.max_tokens = Some(321);
        with.limit_field = limit_field;
        let built = kind.build(&with, &base()).unwrap();
        serde_json::from_slice::<serde_json::Value>(&built.body).unwrap()
    };
    let completion = body(ApiKind::OpenAi, LimitField::MaxCompletionTokens);
    assert_eq!(completion["max_completion_tokens"], 321);
    assert!(completion.get("max_tokens").is_none());
    let plain = body(ApiKind::OpenAi, LimitField::MaxTokens);
    assert_eq!(plain["max_tokens"], 321);
    assert!(plain.get("max_completion_tokens").is_none());
    let anthropic = body(ApiKind::Anthropic, LimitField::MaxCompletionTokens);
    assert_eq!(anthropic["max_tokens"], 321);
    assert!(anthropic.get("max_completion_tokens").is_none());
}

#[test]
fn effort_goes_out_under_each_protocols_native_field_and_is_omitted_when_unset() {
    use crate::model::api::Effort;
    let body = |kind: ApiKind, effort: Option<Effort>| {
        let mut with = settings(kind);
        with.effort = effort;
        let built = kind.build(&with, &base()).unwrap();
        serde_json::from_slice::<serde_json::Value>(&built.body).unwrap()
    };
    let a_none = body(ApiKind::Anthropic, None);
    assert!(a_none.get("output_config").is_none());
    let a_med = body(ApiKind::Anthropic, Some(Effort::Medium));
    assert_eq!(
        a_med["output_config"],
        serde_json::json!({"effort": "medium"})
    );

    let o_none = body(ApiKind::OpenAi, None);
    assert!(o_none.get("reasoning_effort").is_none());
    let o_high = body(ApiKind::OpenAi, Some(Effort::High));
    assert_eq!(o_high["reasoning_effort"], "high");
}

#[test]
fn anthropic_thinking_blocks_and_ids_replay_only_to_the_issuing_provider() {
    let kind = ApiKind::Anthropic;
    let name = alias(kind);
    let issued = anthropic(
        "tool_use",
        &format!(
            r#"{{"type":"thinking","thinking":"review","signature":"SIG-A"}},{{"type":"redacted_thinking","data":"ENC-B"}},{{"type":"thinking","thinking":"unsigned"}},{{"type":"text","text":"Checking now"}},{{"type":"tool_use","id":"toolu_up_1","name":"{name}","input":{{}}}}"#
        ),
    );
    let Ok(ModelStep::Calls {
        state: Some(state), ..
    }) = read(kind, &issued)
    else {
        panic!("expected Anthropic thinking block state");
    };
    assert!(
        !state.contains("unsigned"),
        "unsigned thinking blocks must be dropped"
    );

    let next = |to: &crate::model::api::ApiSettings| -> serde_json::Value {
        let past = exchange(
            "Checking now",
            vec![done_call(
                "c1",
                "agenda.list",
                "{}",
                CallEnd::Done(JsonPayload("{}".into())),
            )],
            Some(state.as_str()),
        );
        let req = request(vec![user("continue")], base().tools, vec![past]);
        serde_json::from_slice(&kind.build(to, &req).unwrap().body).unwrap()
    };

    let same = next(&settings(kind));
    let assistant = same["messages"][1]["content"].as_array().unwrap();
    assert_eq!(assistant[0]["type"], "thinking");
    assert_eq!(assistant[0]["signature"], "SIG-A");
    assert_eq!(assistant[1]["type"], "redacted_thinking");
    assert_eq!(assistant[1]["data"], "ENC-B");
    assert_eq!(assistant[2]["type"], "text");
    assert_eq!(assistant[3]["type"], "tool_use");
    assert_eq!(assistant[3]["id"], "toolu_up_1");
    assert_eq!(
        same["messages"][2]["content"][0]["tool_use_id"],
        "toolu_up_1"
    );

    let mut other_model = settings(kind);
    other_model.model = "model-y".into();
    let mut other_endpoint = settings(kind);
    other_endpoint.endpoint = ApiEndpoint::parse("https://other.example.com/v1").unwrap();
    let mut other_secret = settings(kind);
    other_secret.secret = SecretName::parse("other-key");
    for (what, changed) in [
        ("model", other_model),
        ("endpoint", other_endpoint),
        ("secret", other_secret),
    ] {
        let body = next(&changed);
        let text = body.to_string();
        assert!(
            !text.contains("SIG-A") && !text.contains("ENC-B") && !text.contains("toolu_up_1"),
            "old state leaked after {what} changed: {text}"
        );
        assert_eq!(body["messages"][1]["content"][0]["type"], "text");
        assert_eq!(body["messages"][1]["content"][1]["id"], "t0c0");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "t0c0");
    }

    let huge = "S".repeat(130 * 1024);
    let oversized = anthropic(
        "tool_use",
        &format!(
            r#"{{"type":"thinking","thinking":"","signature":"{huge}"}},{{"type":"tool_use","id":"toolu_big","name":"{name}","input":{{}}}}"#
        ),
    );
    assert_eq!(
        read(kind, &oversized),
        Err(wire::fail("unsupported_tool_signature", false))
    );
}
