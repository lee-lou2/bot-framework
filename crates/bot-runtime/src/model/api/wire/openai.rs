//! OpenAI-compatible Chat Completions (`POST {endpoint}/chat/completions`) request/response codec.
//!
//! Preserves upstream tool call IDs and Gemini `thought_signature` in opaque provider state bound
//! to `ApiSettings::fingerprint` so credentials/signatures never replay across different providers.

use super::{
    Built, MAX_CALLS, args_object, checked_args, conversation, fail, invalid, name_table,
    result_text, role_name, schema_of, token_usage,
};
use crate::model::{
    ModelError, ModelRequest, ModelStep, PendingCall,
    api::{names::NameTable, runner::ApiSettings},
    call_id,
};
use bot_core::command::{JsonPayload, RunOutcome, TokenUsage};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub(super) const PATH: &str = "/chat/completions";
const STATE_KIND: &str = "openai";
const MAX_SIGNATURE: usize = 32 * 1024;
const MAX_ORIGINAL_ID: usize = 128;

pub(super) fn base_headers() -> Vec<(String, String)> {
    vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "application/json".into()),
    ]
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CallState {
    id: Option<String>,
    signature: Option<String>,
}

fn id_ok(id: &str) -> bool {
    (1..=MAX_ORIGINAL_ID).contains(&id.len()) && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

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

/// Decodes provider state only when version, dialect, and issuing provider fingerprint all match.
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
    if let Some(effort) = settings.effort {
        body["reasoning_effort"] = json!(effort.as_str());
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

fn usage_of(usage: Option<&Value>) -> (crate::model::Usage, Option<TokenUsage>) {
    let field = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
    token_usage(field("prompt_tokens"), field("completion_tokens"))
}

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

/// Requires an unambiguous JSON object (as string or object); never fabricates `{}` on missing/null args.
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
        Some("length") => return Err(fail("output_limit", false)),
        Some("content_filter") => return Err(fail("content_filter", false)),
        // Some OpenAI-compatible servers emit `stop` even when returning `tool_calls`.
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
        // Reject duplicate upstream call IDs within a single response to prevent call amplification.
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
