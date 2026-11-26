//! Anthropic Messages API(`POST {endpoint}/v1/messages`, tool use).
//!
//! 요청: `{"model","max_tokens","system"?,"messages":[…],"tools"?:[{"name","description","input_schema"}]}`. 이전 차례는 비서 메시지(`text`·`tool_use` 블록)와 사용자 메시지(`tool_result` 블록)로
//! 이어 붙인다(도구 결과는 사용자 역할이다). 호출 ID는 이 요청 안에서만 쓰는 `t<차례>c<순번>`이다(워커의 멱등 호출 ID와 따로이고, 상류가 준 ID를 되돌려 보낼 필요가 없다).
//!
//! 응답: `stop_reason`이 `end_turn`·`stop_sequence`면 최종 글, `tool_use`면 호출이다. 종료 사유와 블록이 어긋나면(`end_turn`인데 `tool_use` 블록이 있거나 그 반대) 실패다 — 호출은
//! `tool_use`로 끝난 응답에서만 나온다. `max_tokens`(출력 한도로 끊김)·`model_context_window_exceeded`·`refusal`은 부분 글이나 잘린 호출을 쓰지 않고 실패로 끝낸다. 호출이 하나라도
//! 잘못되면(모르는 도구 이름·없거나 객체가 아닌 인자·같은 호출 ID가 두 번) 아무 호출도 내지 않는다.

use super::{
    Built, MAX_CALLS, args_object, checked_args, conversation, fail, invalid, name_table,
    result_text, role_name, schema_of, token_usage,
};
use crate::model::{
    ModelError, ModelRequest, ModelStep, PendingCall, ToolSpec,
    api::{names::NameTable, runner::ApiSettings, transport::Header},
    call_id,
};
use bot_core::command::{JsonPayload, RunOutcome, TokenUsage};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub(super) const PATH: &str = "/v1/messages";
const VERSION: &str = "2023-06-01";
/// 설정에 `max_tokens`가 없을 때(Anthropic은 필수 필드다)
const DEFAULT_MAX_TOKENS: u32 = 4096;
/// 최상위 조합자(`allOf`·`anyOf`·`oneOf`)를 펼칠 때의 중첩 깊이 상한
const MAX_SCHEMA_FLATTEN_DEPTH: usize = 8;

pub(super) fn headers(key: Option<&str>) -> Vec<Header> {
    let mut headers: Vec<Header> = vec![
        ("content-type", "application/json".to_owned(), false),
        ("accept", "application/json".to_owned(), false),
        ("anthropic-version", VERSION.to_owned(), false),
    ];
    if let Some(key) = key {
        headers.push(("x-api-key", key.to_owned(), true));
    }
    headers
}

pub(super) fn build(settings: &ApiSettings, request: &ModelRequest) -> Result<Built, ModelError> {
    let names = name_table(request);
    let mut messages: Vec<Value> = conversation(request)
        .into_iter()
        .map(|turn| json!({"role": role_name(turn.role), "content": turn.text}))
        .collect();
    if messages.is_empty() {
        return Err(fail("invalid_request", false));
    }
    for (turn, exchange) in request.exchanges.iter().enumerate() {
        let mut blocks = Vec::new();
        if !exchange.text.trim().is_empty() {
            blocks.push(json!({"type": "text", "text": exchange.text.trim()}));
        }
        let mut results = Vec::new();
        for (index, call) in exchange.calls.iter().enumerate() {
            let id = format!("t{turn}c{index}");
            let name = names
                .alias(&call.tool)
                .ok_or_else(|| fail("invalid_request", false))?;
            blocks.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": args_object(&call.args),
            }));
            let (text, failed) = result_text(&call.end);
            let mut result = json!({"type": "tool_result", "tool_use_id": id, "content": text});
            if failed {
                result["is_error"] = Value::Bool(true);
            }
            results.push(result);
        }
        if blocks.is_empty() {
            continue;
        }
        messages.push(json!({"role": "assistant", "content": blocks}));
        if !results.is_empty() {
            messages.push(json!({"role": "user", "content": results}));
        }
    }
    let mut body = json!({
        "model": settings.model,
        "max_tokens": settings.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": messages,
    });
    if !request.system.trim().is_empty() {
        body["system"] = Value::String(request.system.clone());
    }
    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                let name = names
                    .alias(&tool.id)
                    .ok_or_else(|| fail("invalid_request", false))?;
                Ok(json!({
                    "name": name,
                    "description": tool.description,
                    "input_schema": input_schema(tool),
                }))
            })
            .collect::<Result<Vec<Value>, ModelError>>()?;
        body["tools"] = Value::Array(tools);
    }
    let body = serde_json::to_vec(&body).map_err(|_| fail("invalid_request", false))?;
    Ok(Built { body, names })
}

/// Anthropic Messages API용 도구 `input_schema`.
/// 상류는 최상위의 `oneOf`·`allOf`·`anyOf`를 거절하므로 최상위 조합자만 풀어 분기의 `properties`와 공통 필수 키(`allOf` 전체·`anyOf`/`oneOf` 교집합)를 끌어올린다.
/// 속성 내부의 중첩 조합자는 그대로 두고, 실제 호출 인자는 브로커가 프로필 원본 스키마 전체로 다시 검증한다.
fn input_schema(tool: &ToolSpec) -> Value {
    let Value::Object(mut root) = schema_of(tool) else {
        return json!({"type": "object"});
    };
    if root.get("type").and_then(Value::as_str) != Some("object") {
        root.insert("type".to_owned(), Value::String("object".to_owned()));
    }
    if !has_top_combinator(&root) {
        return Value::Object(root);
    }
    let root_props: HashSet<String> = root
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default();
    let mut properties = root
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut required = required_of(&root);
    let mut extra_props = Map::new();
    let mut extra_req = Vec::new();
    flatten_combinators(&mut root, &mut extra_props, &mut extra_req, 0);
    for (name, schema) in extra_props {
        if !root_props.contains(&name) {
            merge_property(&mut properties, name, schema);
        }
    }
    push_unique(&mut required, extra_req);
    if !properties.is_empty() {
        root.insert("properties".to_owned(), Value::Object(properties));
    }
    if !required.is_empty() {
        root.insert(
            "required".to_owned(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(root)
}

fn has_top_combinator(map: &Map<String, Value>) -> bool {
    map.contains_key("allOf") || map.contains_key("anyOf") || map.contains_key("oneOf")
}

fn required_of(map: &Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(items) = map.get("required").and_then(Value::as_array) {
        for item in items {
            if let Some(name) = item.as_str()
                && !out.iter().any(|existing| existing == name)
            {
                out.push(name.to_owned());
            }
        }
    }
    out
}

fn push_unique(target: &mut Vec<String>, items: Vec<String>) {
    for item in items {
        if !target.contains(&item) {
            target.push(item);
        }
    }
}

fn flatten_combinators(
    obj: &mut Map<String, Value>,
    props: &mut Map<String, Value>,
    req: &mut Vec<String>,
    depth: usize,
) {
    let all_of = obj.remove("allOf");
    let any_of = obj.remove("anyOf");
    let one_of = obj.remove("oneOf");
    if depth >= MAX_SCHEMA_FLATTEN_DEPTH {
        return;
    }
    if let Some(Value::Array(branches)) = all_of {
        for branch in branches {
            if let Some((b_props, b_req)) = extract_branch(branch, depth) {
                for (name, schema) in b_props {
                    merge_property(props, name, schema);
                }
                push_unique(req, b_req);
            }
        }
    }
    for group in [any_of, one_of].into_iter().flatten() {
        let Value::Array(branches) = group else {
            continue;
        };
        let mut common: Option<Vec<String>> = None;
        let mut all_objects = !branches.is_empty();
        for branch in branches {
            if let Some((b_props, b_req)) = extract_branch(branch, depth) {
                for (name, schema) in b_props {
                    merge_property(props, name, schema);
                }
                match &mut common {
                    None => common = Some(b_req),
                    Some(prev) => prev.retain(|name| b_req.contains(name)),
                }
            } else {
                all_objects = false;
            }
        }
        if all_objects && let Some(common_req) = common {
            push_unique(req, common_req);
        }
    }
}

fn extract_branch(branch: Value, depth: usize) -> Option<(Map<String, Value>, Vec<String>)> {
    if depth >= MAX_SCHEMA_FLATTEN_DEPTH {
        return None;
    }
    let Value::Object(mut obj) = branch else {
        return None;
    };
    let mut props = match obj.remove("properties") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let mut req = required_of(&obj);
    flatten_combinators(&mut obj, &mut props, &mut req, depth + 1);
    Some((props, req))
}

fn is_pure_any_of(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.len() == 1 && map.get("anyOf").is_some_and(Value::is_array))
}

fn merge_property(props: &mut Map<String, Value>, name: String, incoming: Value) {
    let Some(existing) = props.get_mut(&name) else {
        props.insert(name, incoming);
        return;
    };
    if *existing == incoming {
        return;
    }
    let mut variants = if is_pure_any_of(existing) {
        existing
            .as_object_mut()
            .and_then(|map| map.remove("anyOf"))
            .and_then(|value| match value {
                Value::Array(items) => Some(items),
                _ => None,
            })
            .unwrap_or_default()
    } else {
        vec![existing.take()]
    };
    let add = if is_pure_any_of(&incoming) {
        match incoming {
            Value::Object(mut map) => match map.remove("anyOf") {
                Some(Value::Array(items)) => items,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    } else {
        vec![incoming]
    };
    for candidate in add {
        if !variants.contains(&candidate) {
            variants.push(candidate);
        }
    }
    *existing = json!({"anyOf": variants});
}

/// 응답의 `usage`에서 (관측용, 실측). 입력은 `input_tokens`에 캐시 읽기·쓰기를 더한 값이다(CLI 실행기와 같은 규칙).
fn usage_of(usage: Option<&Value>) -> (crate::model::Usage, Option<TokenUsage>) {
    let field = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
    let prompt = field("input_tokens").map(|input| {
        input
            .saturating_add(field("cache_read_input_tokens").unwrap_or(0))
            .saturating_add(field("cache_creation_input_tokens").unwrap_or(0))
    });
    token_usage(prompt, field("output_tokens"))
}

pub(super) fn parse(
    value: &Value,
    request: &ModelRequest,
    names: &NameTable,
) -> Result<ModelStep, ModelError> {
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let stop = value
        .get("stop_reason")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    match stop {
        // 출력 한도로 끊긴 글·호출은 쓰지 않는다(부분 글을 성공으로 올리지 않고, 잘린 호출은 실행하지 않는다)
        "max_tokens" => return Err(fail("output_limit", false)),
        "model_context_window_exceeded" => return Err(fail("context_limit", false)),
        "refusal" => return Err(fail("refused", false)),
        "end_turn" | "stop_sequence" | "tool_use" => {}
        _ => return Err(invalid()),
    }
    let (usage, measured) = usage_of(value.get("usage"));
    let mut texts: Vec<&str> = Vec::new();
    let mut uses: Vec<&Value> = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => texts.push(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?,
            ),
            Some("tool_use") => uses.push(block),
            // 생각·서버 도구 같은 다른 블록은 이 실행기가 요청하지 않았다: 읽지 않는다
            _ => {}
        }
    }
    let text = texts.join("\n\n");
    // 호출은 `tool_use`로 끝난 응답에서만 나온다: 종료 사유와 블록이 어긋난 응답은 실행 계획을 만들지 않는다
    if uses.is_empty() == (stop == "tool_use") {
        return Err(invalid());
    }
    if uses.is_empty() {
        return Ok(ModelStep::Final {
            text,
            usage,
            outcome: RunOutcome {
                truncated: false,
                usage: measured,
            },
        });
    }
    if uses.len() > MAX_CALLS {
        return Err(fail("too_many_calls", false));
    }
    let turn = request.exchanges.len();
    let mut calls = Vec::with_capacity(uses.len());
    // 상류가 준 호출 ID는 쓰지 않지만 한 응답 안에서 겹치면 같은 호출을 둘로 부풀린 것이라 거절한다
    let mut upstream_ids: HashSet<&str> = HashSet::new();
    for (index, block) in uses.into_iter().enumerate() {
        match block.get("id") {
            None | Some(Value::Null) => {}
            Some(Value::String(id)) => {
                if !upstream_ids.insert(id.as_str()) {
                    return Err(invalid());
                }
            }
            Some(_) => return Err(invalid()),
        }
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let tool = names
            .tool(name)
            .ok_or_else(|| fail("unknown_tool", false))?;
        let input = block.get("input").cloned().unwrap_or(Value::Null);
        calls.push(PendingCall {
            call: call_id(&request.attempt.run, turn, index),
            tool: tool.clone(),
            args: JsonPayload(checked_args(input)?),
        });
    }
    Ok(ModelStep::Calls {
        text,
        calls,
        usage,
        measured,
        state: None,
    })
}
