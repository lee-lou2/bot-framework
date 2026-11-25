//! 응답을 엄격하게 읽는 규칙(검토에서 나온 것): 호출 인자는 있고 객체이며 모호하지 않아야 하고, 한 응답에서 같은 상류 호출 ID는 한 번뿐이며, Anthropic은 `tool_use` 종료일 때만 호출이고,
//! 공급자 상태(호출 ID·서명)는 그것을 준 공급자에게만 되돌아간다. 어느 것이든 어기면 호출을 하나도 내지 않는다.

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
        vec![user("오늘 일정 알려줘")],
        vec![tool(
            "agenda.list",
            "일정 목록을 조회한다",
            r#"{"type":"object"}"#,
        )],
        vec![],
    )
}

/// 이 요청에서 `agenda.list`가 모델에게 보이는 이름
fn alias(kind: ApiKind) -> String {
    let built = kind.build(&settings(kind), &base()).unwrap();
    built
        .names
        .alias(&ToolId("agenda.list".into()))
        .unwrap()
        .to_owned()
}

/// 응답 본문 글을 그대로(중복 키처럼 값으로는 못 만드는 모양도) 읽는다.
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
        other => panic!("호출이어야 해요: {other:?}"),
    }
}

#[test]
fn a_tool_call_without_usable_arguments_runs_nothing() {
    let name = alias(ApiKind::OpenAi);
    let none = failed("invalid_tool_args", false);
    // OpenAI 호환: 빈 글·공백·없음·null·객체 아님·중복 키(바깥·안쪽·객체 모양)
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
    // Anthropic: `input` 없음·null·문자열·배열·중복 키
    let name = alias(ApiKind::Anthropic);
    for input in [
        "",
        r#","input":null"#,
        r#","input":"문자열""#,
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
    // 대조: 빈 객체는 인자가 있는 호출이다
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
    // 쓸 수 없는 모양(공백이 든 ID)이라도 겹치면 같다
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
    // 대조: `tool_use` 종료는 호출이고, 호출 없는 `end_turn`은 최종 글이다
    assert_eq!(
        call_count(read(ApiKind::Anthropic, &anthropic("tool_use", &block))),
        1
    );
    let text = anthropic("end_turn", r#"{"type":"text","text":"끝"}"#);
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
        panic!("상류가 준 상태가 있어야 해요");
    };
    // 이 상태를 단 지난 차례가 있는 다음 요청을 `to`의 설정으로 만든다
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
        let req = request(vec![user("계속해")], base().tools, vec![past]);
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
        ("모델", other_model),
        ("주소", other_endpoint),
        ("자격 이름", other_secret),
    ] {
        let body = next(&changed);
        assert!(
            !body.contains("call_up_1") && !body.contains("SIG-ONE"),
            "{what}이 바뀐 공급자에게 옛 공급자의 ID·서명이 갔다: {body}"
        );
        assert!(
            body.contains("tool_call_id"),
            "{what}: 결과는 여전히 호출과 이어져야 해요"
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
    // OpenAI 호환: 적힌 이름 그대로, 다른 이름은 보내지 않는다(새 OpenAI 모델은 `max_tokens`를 거절한다)
    let completion = body(ApiKind::OpenAi, LimitField::MaxCompletionTokens);
    assert_eq!(completion["max_completion_tokens"], 321);
    assert!(completion.get("max_tokens").is_none());
    let plain = body(ApiKind::OpenAi, LimitField::MaxTokens);
    assert_eq!(plain["max_tokens"], 321);
    assert!(plain.get("max_completion_tokens").is_none());
    // Anthropic은 언제나 `max_tokens`다
    let anthropic = body(ApiKind::Anthropic, LimitField::MaxCompletionTokens);
    assert_eq!(anthropic["max_tokens"], 321);
    assert!(anthropic.get("max_completion_tokens").is_none());
}
