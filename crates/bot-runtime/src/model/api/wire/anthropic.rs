//! Anthropic Messages API (`POST {endpoint}/v1/messages`) wire serialization and response parsing.

use super::{
    Built, MAX_CALLS, args_object, checked_args, conversation, fail, invalid, name_table,
    result_text, role_name, schema_of, token_usage,
};
use crate::model::{
    ModelError, ModelRequest, ModelStep, PendingCall, ToolSpec,
    api::{names::NameTable, runner::ApiSettings},
    call_id,
};
use bot_core::command::{JsonPayload, RunOutcome, TokenUsage};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub(super) const PATH: &str = "/v1/messages";
const VERSION: &str = "2023-06-01";
/// Required by Anthropic Messages API when `max_tokens` is unset.
const DEFAULT_MAX_TOKENS: u32 = 4096;
const MAX_SCHEMA_FLATTEN_DEPTH: usize = 8;
const STATE_KIND: &str = "anthropic";
const MAX_ORIGINAL_ID: usize = 128;
const MAX_THINKING_BLOCKS: usize = 16;
/// Caps opaque provider state per turn to stay well under the 1 MiB checkpoint limit.
const MAX_STATE_BYTES: usize = 128 * 1024;

fn id_ok(id: &str) -> bool {
    (1..=MAX_ORIGINAL_ID).contains(&id.len()) && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn thinking_block(block: &Value) -> Option<Value> {
    match block.get("type").and_then(Value::as_str) {
        Some("thinking") => {
            let thinking = block.get("thinking").and_then(Value::as_str)?;
            let signature = block.get("signature").and_then(Value::as_str)?;
            (!signature.is_empty())
                .then(|| json!({"type": "thinking", "thinking": thinking, "signature": signature}))
        }
        Some("redacted_thinking") => {
            let data = block.get("data").and_then(Value::as_str)?;
            (!data.is_empty()).then(|| json!({"type": "redacted_thinking", "data": data}))
        }
        _ => None,
    }
}

struct TurnState {
    thinking: Vec<Value>,
    ids: Vec<Option<String>>,
}

fn encode_state(
    thinking: &[Value],
    ids: &[Option<String>],
    provider: &str,
) -> Result<Option<String>, ModelError> {
    if thinking.len() > MAX_THINKING_BLOCKS {
        return Err(fail("unsupported_tool_signature", false));
    }
    let has_ids = ids.iter().any(Option::is_some);
    if thinking.is_empty() && !has_ids {
        return Ok(None);
    }
    let mut obj = Map::new();
    obj.insert("v".to_owned(), json!(1));
    obj.insert("k".to_owned(), json!(STATE_KIND));
    obj.insert("p".to_owned(), json!(provider));
    if !thinking.is_empty() {
        obj.insert("b".to_owned(), Value::Array(thinking.to_vec()));
    }
    if has_ids {
        obj.insert(
            "c".to_owned(),
            Value::Array(
                ids.iter()
                    .map(|id| {
                        id.as_deref()
                            .map_or(Value::Null, |s| Value::String(s.into()))
                    })
                    .collect(),
            ),
        );
    }
    let encoded = Value::Object(obj).to_string();
    if encoded.len() > MAX_STATE_BYTES {
        return Err(fail("unsupported_tool_signature", false));
    }
    Ok(Some(encoded))
}

fn decode_state(state: Option<&str>, calls: usize, provider: &str) -> TurnState {
    let mut out = TurnState {
        thinking: Vec::new(),
        ids: vec![None; calls],
    };
    let Some(value) = state.and_then(|text| serde_json::from_str::<Value>(text).ok()) else {
        return out;
    };
    if value.get("v").and_then(Value::as_u64) != Some(1)
        || value.get("k").and_then(Value::as_str) != Some(STATE_KIND)
        || value.get("p").and_then(Value::as_str) != Some(provider)
    {
        return out;
    }
    if let Some(items) = value.get("b").and_then(Value::as_array) {
        out.thinking = items
            .iter()
            .take(MAX_THINKING_BLOCKS)
            .filter_map(thinking_block)
            .collect();
    }
    if let Some(items) = value.get("c").and_then(Value::as_array) {
        for (slot, item) in out.ids.iter_mut().zip(items) {
            *slot = item.as_str().filter(|id| id_ok(id)).map(str::to_owned);
        }
    }
    out
}

fn pick_id(saved: Option<&str>, turn: usize, index: usize, used: &mut HashSet<String>) -> String {
    if let Some(id) = saved
        && used.insert(id.to_owned())
    {
        return id.to_owned();
    }
    let mut candidate = format!("t{turn}c{index}");
    let mut round = 0u32;
    while !used.insert(candidate.clone()) {
        round += 1;
        candidate = format!("t{turn}c{index}_{round}");
    }
    candidate
}

pub(super) fn base_headers() -> Vec<(String, String)> {
    vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "application/json".into()),
        ("anthropic-version".into(), VERSION.into()),
    ]
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
    let mut used: HashSet<String> = HashSet::new();
    let provider = settings.fingerprint();
    for (turn, exchange) in request.exchanges.iter().enumerate() {
        let mut blocks = Vec::new();
        let state = if exchange.calls.is_empty() {
            TurnState {
                thinking: Vec::new(),
                ids: Vec::new(),
            }
        } else {
            decode_state(exchange.state.as_deref(), exchange.calls.len(), &provider)
        };
        blocks.extend(state.thinking);
        if !exchange.text.trim().is_empty() {
            blocks.push(json!({"type": "text", "text": exchange.text.trim()}));
        }
        let mut results = Vec::new();
        for (index, (call, saved_id)) in exchange.calls.iter().zip(state.ids).enumerate() {
            let id = pick_id(saved_id.as_deref(), turn, index, &mut used);
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
    if let Some(effort) = settings.effort {
        body["output_config"] = json!({"effort": effort.as_str()});
    }
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

/// Flattens top-level `allOf`/`anyOf`/`oneOf` combinators rejected by Anthropic while keeping nested ones intact;
/// the broker still validates actual call arguments against the original full schema.
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

/// Counts prompt tokens as `input_tokens + cache_read_input_tokens + cache_creation_input_tokens` (matching CLI usage accounting).
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
    settings: &ApiSettings,
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
        // Never treat truncated or refused output as a valid answer or executable tool plan.
        "max_tokens" => return Err(fail("output_limit", false)),
        "model_context_window_exceeded" => return Err(fail("context_limit", false)),
        "refusal" => return Err(fail("refused", false)),
        "end_turn" | "stop_sequence" | "tool_use" => {}
        _ => return Err(invalid()),
    }
    let (usage, measured) = usage_of(value.get("usage"));
    let mut texts: Vec<&str> = Vec::new();
    let mut uses: Vec<&Value> = Vec::new();
    let mut thinking: Vec<Value> = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => texts.push(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?,
            ),
            Some("tool_use") => uses.push(block),
            Some("thinking" | "redacted_thinking") => {
                if let Some(saved) = thinking_block(block) {
                    thinking.push(saved);
                }
            }
            // Ignore unrequested server-side content blocks.
            _ => {}
        }
    }
    let text = texts.join("\n\n");
    // Tool calls are accepted only when `stop_reason == "tool_use"` and vice versa.
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
    let mut call_ids = Vec::with_capacity(uses.len());
    let mut upstream_ids: HashSet<&str> = HashSet::new();
    for (index, block) in uses.into_iter().enumerate() {
        let original = match block.get("id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => {
                if !upstream_ids.insert(id.as_str()) {
                    return Err(invalid());
                }
                id_ok(id).then(|| id.clone())
            }
            Some(_) => return Err(invalid()),
        };
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let tool = names
            .tool(name)
            .ok_or_else(|| fail("unknown_tool", false))?;
        let input = block.get("input").cloned().unwrap_or(Value::Null);
        call_ids.push(original);
        calls.push(PendingCall {
            call: call_id(&request.attempt.run, turn, index),
            tool: tool.clone(),
            args: JsonPayload(checked_args(input)?),
        });
    }
    let state = encode_state(&thinking, &call_ids, &settings.fingerprint())?;
    Ok(ModelStep::Calls {
        text,
        calls,
        usage,
        measured,
        state,
    })
}
