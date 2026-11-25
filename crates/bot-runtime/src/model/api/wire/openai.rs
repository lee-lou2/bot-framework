//! OpenAI 호환 Chat Completions(`POST {endpoint}/chat/completions`, tool_calls). OpenAI·Gemini 호환·Ollama·vLLM이 같은 모양을 쓴다.
//!
//! 요청: `{"model","messages":[system?,…대화…,비서(tool_calls),tool,…],"tools"?:[{"type":"function","function":{…}}],"max_tokens"?}`. 시스템 글만 `system` 역할이고, 사용자 줄·도구 결과는 `user`·`tool`
//! 역할이다. 이전 차례의 호출 ID는 상류가 처음 준 ID를 되돌려 보낸다(엄격한 서버가 자기가 발급하지 않은 ID를 거절하므로). 없거나 겹치면 이 요청 안에서만 쓰는 `call_<차례>_<순번>`이다.
//!
//! 응답: 첫 `choices[0]`만 읽는다. `finish_reason`이 `length`면 부분 글이나 잘린 호출을 쓰지 않고 실패(`output_limit`), `content_filter`면 `content_filter`다. 호출(`tool_calls`)이 있으면 종료 사유가
//! `stop`이어도 호출로 읽는다(그렇게 답하는 호환 서버가 있다). `arguments`는 있어야 하고 JSON 문자열이면 중복 키 없이 객체로 풀려야 한다(객체로 주는 서버는 객체도 받는다). 같은 호출 ID가 한 응답에 두 번
//! 나오면 거절한다. 출력 상한은 설정에 적힌 이름(`max_completion_tokens` 또는 `max_tokens`) 그대로 보낸다.
//!
//! **공급자 상태**: Gemini 계열은 호출과 함께 `extra_content.google.thought_signature`를 주고 다음 요청의 같은 호출 자리에 되돌려야 한다(빠지면 후속 요청이 400이다). 이 서명과 상류가 준 호출 ID를
//! 호출 순서대로 `state`(불투명 글)에 담아 워커가 체크포인트에 적어 두었다가 승인 대기 뒤 재개에서도 같은 요청에 되돌린다. 서명이 너무 크면 호출을 하나도 부르지 않고 실패한다.
//! 상태에는 그것을 준 공급자의 지문(종류·주소·모델·자격 이름, `ApiSettings::fingerprint`)을 묶는다: 대기 동안 같은 별칭의 목적지가 바뀌면 옛 공급자의 ID·서명을 새 공급자에 싣지 않는다
//! (호출 ID는 이 요청 안에서만 쓰는 값으로 새로 만들고 서명은 뺀다. 서명이 필요한 모델이면 그 상류가 요청을 거절하고 고정 코드로 끝난다).

use super::{
    Built, MAX_CALLS, args_object, checked_args, conversation, fail, invalid, name_table,
    result_text, role_name, schema_of, token_usage,
};
use crate::model::{
    ModelError, ModelRequest, ModelStep, PendingCall,
    api::{names::NameTable, runner::ApiSettings, transport::Header},
    call_id,
};
use bot_core::command::{JsonPayload, RunOutcome, TokenUsage};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub(super) const PATH: &str = "/chat/completions";
/// 불투명 공급자 상태의 형식 표지(다른 종류의 실행기가 남긴 상태는 읽지 않는다)
const STATE_KIND: &str = "openai";
/// 호출 하나의 서명 크기 상한
const MAX_SIGNATURE: usize = 32 * 1024;
/// 상류가 준 호출 ID의 길이 상한
const MAX_ORIGINAL_ID: usize = 128;

pub(super) fn headers(key: Option<&str>) -> Vec<Header> {
    let mut headers: Vec<Header> = vec![
        ("content-type", "application/json".to_owned(), false),
        ("accept", "application/json".to_owned(), false),
    ];
    if let Some(key) = key {
        headers.push(("authorization", format!("Bearer {key}"), true));
    }
    headers
}

/// 호출 하나에 대해 상류가 준 것: 원래 호출 ID와 사고 서명(Gemini)
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CallState {
    id: Option<String>,
    signature: Option<String>,
}

/// 상류가 준 ID를 그대로 되돌려도 되는가: 보이는 ASCII만이고 길이가 적당하다(줄바꿈·공백이 든 값은 쓰지 않는다).
fn id_ok(id: &str) -> bool {
    (1..=MAX_ORIGINAL_ID).contains(&id.len()) && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// 호출들의 상태를 `{"v":1,"k":"openai","p":"<공급자 지문>","c":[{"i":"<ID>","s":"<서명>"},…]}`로 쓴다. 담을 것이 없으면 `None`이다.
fn encode_state(calls: &[CallState], provider: &str) -> Option<String> {
    if calls
        .iter()
        .all(|call| call.id.is_none() && call.signature.is_none())
    {
        return None;
    }
    let entries: Vec<Value> = calls
        .iter()
        .map(|call| {
            let mut entry = Map::new();
            if let Some(id) = &call.id {
                entry.insert("i".to_owned(), Value::String(id.clone()));
            }
            if let Some(signature) = &call.signature {
                entry.insert("s".to_owned(), Value::String(signature.clone()));
            }
            Value::Object(entry)
        })
        .collect();
    Some(json!({"v": 1, "k": STATE_KIND, "p": provider, "c": entries}).to_string())
}

/// `encode_state`의 역: 호출 `calls`개에 맞춘다. 읽을 수 없거나 다른 종류의 상태이거나 **다른 공급자가 준 상태**(`provider`가 다르다)면 모두 비어 있다(받아들이지 않는다).
fn decode_state(state: Option<&str>, calls: usize, provider: &str) -> Vec<CallState> {
    let mut out = vec![CallState::default(); calls];
    let Some(value) = state.and_then(|text| serde_json::from_str::<Value>(text).ok()) else {
        return out;
    };
    if value.get("v").and_then(Value::as_u64) != Some(1)
        || value.get("k").and_then(Value::as_str) != Some(STATE_KIND)
        || value.get("p").and_then(Value::as_str) != Some(provider)
    {
        return out;
    }
    let Some(entries) = value.get("c").and_then(Value::as_array) else {
        return out;
    };
    for (slot, entry) in out.iter_mut().zip(entries) {
        slot.id = entry
            .get("i")
            .and_then(Value::as_str)
            .filter(|id| id_ok(id))
            .map(str::to_owned);
        slot.signature = entry
            .get("s")
            .and_then(Value::as_str)
            .filter(|signature| signature.len() <= MAX_SIGNATURE)
            .map(str::to_owned);
    }
    out
}

/// 이 요청의 호출 ID: 상류가 준 ID(겹치지 않을 때), 아니면 `call_<차례>_<순번>`(겹치면 뒤에 번호를 더한다).
fn pick_id(state: &CallState, turn: usize, index: usize, used: &mut HashSet<String>) -> String {
    if let Some(id) = &state.id
        && used.insert(id.clone())
    {
        return id.clone();
    }
    let mut candidate = format!("call_{turn}_{index}");
    let mut round = 0u32;
    while !used.insert(candidate.clone()) {
        round += 1;
        candidate = format!("call_{turn}_{index}_{round}");
    }
    candidate
}

pub(super) fn build(settings: &ApiSettings, request: &ModelRequest) -> Result<Built, ModelError> {
    let names = name_table(request);
    let turns = conversation(request);
    if turns.is_empty() {
        return Err(fail("invalid_request", false));
    }
    let mut messages: Vec<Value> = Vec::new();
    if !request.system.trim().is_empty() {
        messages.push(json!({"role": "system", "content": request.system}));
    }
    messages.extend(
        turns
            .into_iter()
            .map(|turn| json!({"role": role_name(turn.role), "content": turn.text})),
    );
    let mut used: HashSet<String> = HashSet::new();
    let provider = settings.fingerprint();
    for (turn, exchange) in request.exchanges.iter().enumerate() {
        if exchange.calls.is_empty() {
            continue;
        }
        let states = decode_state(exchange.state.as_deref(), exchange.calls.len(), &provider);
        let mut tool_calls = Vec::new();
        let mut results = Vec::new();
        for (index, (call, state)) in exchange.calls.iter().zip(&states).enumerate() {
            let id = pick_id(state, turn, index, &mut used);
            let name = names
                .alias(&call.tool)
                .ok_or_else(|| fail("invalid_request", false))?;
            let mut item = json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args_object(&call.args).to_string()},
            });
            if let Some(signature) = &state.signature {
                item["extra_content"] = json!({"google": {"thought_signature": signature}});
            }
            tool_calls.push(item);
            let (text, _) = result_text(&call.end);
            results.push(json!({"role": "tool", "tool_call_id": id, "content": text}));
        }
        let content = if exchange.text.trim().is_empty() {
            Value::Null
        } else {
            Value::String(exchange.text.trim().to_owned())
        };
        messages.push(json!({"role": "assistant", "content": content, "tool_calls": tool_calls}));
        messages.extend(results);
    }
    let mut body = json!({"model": settings.model, "messages": messages});
    if let Some(limit) = settings.max_tokens {
        body[settings.limit_field.name()] = json!(limit);
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
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": tool.description,
                        "parameters": schema_of(tool),
                    },
                }))
            })
            .collect::<Result<Vec<Value>, ModelError>>()?;
        body["tools"] = Value::Array(tools);
    }
    let body = serde_json::to_vec(&body).map_err(|_| fail("invalid_request", false))?;
    Ok(Built { body, names })
}

/// 응답의 `usage`에서 (관측용, 실측). `prompt_tokens`는 캐시된 입력을 이미 포함한다.
fn usage_of(usage: Option<&Value>) -> (crate::model::Usage, Option<TokenUsage>) {
    let field = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
    token_usage(field("prompt_tokens"), field("completion_tokens"))
}

/// 메시지의 글: 문자열·`null`·글 조각 배열(이어 붙인다)을 받는다.
fn content_text(content: Option<&Value>) -> Result<String, ModelError> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => Ok(parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n")),
        Some(_) => Err(invalid()),
    }
}

/// 호출의 `arguments`: JSON 문자열(중복 키 없이 객체로 풀려야 한다)이나 객체만 받는다. 없음·빈 글·`null`·객체가 아닌 값은 빈 객체로 짐작하지 않고 `invalid_tool_args`다.
fn call_args(arguments: Option<&Value>) -> Result<String, ModelError> {
    match arguments {
        Some(Value::String(text)) => match bot_core::json::parse(text) {
            Some(value) => checked_args(value),
            None => Err(fail("invalid_tool_args", false)),
        },
        Some(object) if object.is_object() => checked_args(object.clone()),
        _ => Err(fail("invalid_tool_args", false)),
    }
}

pub(super) fn parse(
    settings: &ApiSettings,
    value: &Value,
    request: &ModelRequest,
    names: &NameTable,
) -> Result<ModelStep, ModelError> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(invalid)?;
    let message = choice.get("message").ok_or_else(invalid)?;
    match choice.get("finish_reason").and_then(Value::as_str) {
        // 출력 한도로 끊긴 글·호출은 쓰지 않는다(잘린 호출은 실행하지 않는다)
        Some("length") => return Err(fail("output_limit", false)),
        Some("content_filter") => return Err(fail("content_filter", false)),
        Some("stop" | "tool_calls") => {}
        _ => return Err(invalid()),
    }
    let (usage, measured) = usage_of(value.get("usage"));
    let text = content_text(message.get("content"))?;
    let tool_calls: &[Value] = match message.get("tool_calls") {
        None | Some(Value::Null) => &[],
        Some(Value::Array(items)) => items,
        Some(_) => return Err(invalid()),
    };
    if tool_calls.is_empty() {
        let refusal = message
            .get("refusal")
            .and_then(Value::as_str)
            .is_some_and(|refusal| !refusal.trim().is_empty());
        if refusal && text.trim().is_empty() {
            return Err(fail("refused", false));
        }
        return Ok(ModelStep::Final {
            text,
            usage,
            outcome: RunOutcome {
                truncated: false,
                usage: measured,
            },
        });
    }
    if tool_calls.len() > MAX_CALLS {
        return Err(fail("too_many_calls", false));
    }
    let turn = request.exchanges.len();
    let mut calls = Vec::with_capacity(tool_calls.len());
    let mut states = Vec::with_capacity(tool_calls.len());
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for (index, item) in tool_calls.iter().enumerate() {
        if item
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "function")
        {
            return Err(invalid());
        }
        let function = item.get("function").ok_or_else(invalid)?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let tool = names
            .tool(name)
            .ok_or_else(|| fail("unknown_tool", false))?;
        let args = call_args(function.get("arguments"))?;
        let signature = item
            .pointer("/extra_content/google/thought_signature")
            .and_then(Value::as_str);
        if signature.is_some_and(|signature| signature.len() > MAX_SIGNATURE) {
            return Err(fail("unsupported_tool_signature", false));
        }
        // 상류가 준 호출 ID: 한 응답 안에서 겹치면 같은 호출을 둘로 부풀린 것이라 거절하고, 쓸 수 있는 모양일 때만 되돌릴 상태에 담는다
        let upstream_id = match item.get("id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => Some(id.as_str()),
            Some(_) => return Err(invalid()),
        };
        if upstream_id.is_some_and(|id| !seen_ids.insert(id)) {
            return Err(invalid());
        }
        let original = upstream_id.filter(|id| id_ok(id));
        states.push(CallState {
            id: original.map(str::to_owned),
            signature: signature.map(str::to_owned),
        });
        calls.push(PendingCall {
            call: call_id(&request.attempt.run, turn, index),
            tool: tool.clone(),
            args: JsonPayload(args),
        });
    }
    Ok(ModelStep::Calls {
        text,
        calls,
        usage,
        measured,
        state: encode_state(&states, &settings.fingerprint()),
    })
}
