//! OpenAI-compatible JSON completion, SSE chunk, model object, and fixed service notice formatters.
use bot_core::{api::ApprovalBlock, command::TokenUsage};
use serde_json::{Map, Value, json};

pub const OWNED_BY: &str = "bot-framework";
pub const KEEP_ALIVE: &str = ": keep-alive\n\n";
pub const DONE: &str = "data: [DONE]\n\n";
const DELTA_BYTES: usize = 4096;

const RETRY_HINT: &str = "Query again with the same Idempotency-Key and request.";

pub fn approval_pending() -> String {
    format!("Awaiting approval. {RETRY_HINT}")
}

pub fn still_running() -> String {
    format!("Request is still in progress. {RETRY_HINT}")
}

/// Host-authored notice when human approval cannot be obtained; never includes raw internal codes.
pub fn blocked(reason: ApprovalBlock) -> String {
    let why = match reason {
        ApprovalBlock::NoApprover => "No approver is configured",
        ApprovalBlock::ApproverRevoked => "The configured approver is no longer valid",
        ApprovalBlock::RouteChanged => "The approval route changed",
        ApprovalBlock::ApprovalDmUnavailable => "The approval request could not be delivered",
    };
    format!("{why}. The action requiring approval was not executed.")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Finish {
    Stop,
    Length,
}

impl Finish {
    pub fn name(self) -> &'static str {
        match self {
            Finish::Stop => "stop",
            Finish::Length => "length",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Identity {
    pub id: String,
    /// First-admission Unix timestamp in seconds from the database clock.
    pub created: u64,
    pub model: String,
}

impl Identity {
    pub fn new(run: &str, created: u64, model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{run}"),
            created,
            model: model.to_owned(),
        }
    }
}

fn usage_value(usage: TokenUsage) -> Value {
    json!({
        "prompt_tokens": usage.prompt,
        "completion_tokens": usage.completion,
        "total_tokens": usage.prompt.saturating_add(usage.completion),
    })
}

pub fn completion(
    identity: &Identity,
    content: &str,
    finish: Finish,
    usage: Option<TokenUsage>,
) -> Value {
    let mut body = json!({
        "id": identity.id,
        "object": "chat.completion",
        "created": identity.created,
        "model": identity.model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content, "refusal": null},
            "finish_reason": finish.name(),
            "logprobs": null,
        }],
    });
    if let Some(usage) = usage {
        body["usage"] = usage_value(usage);
    }
    body
}

fn chunk(identity: &Identity, choices: Value, usage: Option<Value>) -> String {
    let mut body = Map::new();
    body.insert("id".into(), identity.id.clone().into());
    body.insert("object".into(), "chat.completion.chunk".into());
    body.insert("created".into(), identity.created.into());
    body.insert("model".into(), identity.model.clone().into());
    body.insert("choices".into(), choices);
    if let Some(usage) = usage {
        body.insert("usage".into(), usage);
    }
    event(&Value::Object(body))
}

pub fn event(value: &Value) -> String {
    format!("data: {value}\n\n")
}

pub fn first_chunk(identity: &Identity, include_usage: bool) -> String {
    chunk(
        identity,
        json!([{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]),
        include_usage.then_some(Value::Null),
    )
}

pub fn delta_chunks(identity: &Identity, text: &str, include_usage: bool) -> Vec<String> {
    pieces(text, DELTA_BYTES)
        .into_iter()
        .map(|piece| {
            chunk(
                identity,
                json!([{"index": 0, "delta": {"content": piece}, "finish_reason": null}]),
                include_usage.then_some(Value::Null),
            )
        })
        .collect()
}

pub fn finish_chunk(identity: &Identity, finish: Finish, include_usage: bool) -> String {
    chunk(
        identity,
        json!([{"index": 0, "delta": {}, "finish_reason": finish.name()}]),
        include_usage.then_some(Value::Null),
    )
}

pub fn usage_chunk(identity: &Identity, usage: TokenUsage) -> String {
    chunk(identity, json!([]), Some(usage_value(usage)))
}

/// Splits `text` into chunks of at most `max` bytes on UTF-8 char boundaries.
pub fn pieces(text: &str, max: usize) -> Vec<&str> {
    // Clamp to at least 4 bytes so any single UTF-8 scalar fits and the loop terminates.
    let max = max.max(4);
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(max);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (head, tail) = rest.split_at(end);
        out.push(head);
        rest = tail;
    }
    out
}

pub fn model(id: &str, created: u64) -> Value {
    json!({"id": id, "object": "model", "created": created, "owned_by": OWNED_BY})
}

pub fn model_list(models: Vec<Value>) -> Value {
    json!({"object": "list", "data": models})
}

#[cfg(test)]
mod tests;
