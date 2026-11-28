//! Shared fixtures for direct API model runner tests: secret store and active-check fakes, dual-dialect reply builders, and runner assembly.
#![allow(dead_code)]

use super::upstream::{FakeApi, Reply, Seen};
use bot_core::{
    active::ActiveCheck,
    id::{ProfileId, SecretName},
    port::{Error, ProfileSecrets, Reply as Port, SecretStore, SecretValue},
};
use bot_runtime::model::{ApiEndpoint, ApiKind, ApiRunner, ApiSettings};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

pub const KEY: &str = "sk-test-0123456789abcdef";

pub struct Keys {
    values: BTreeMap<String, Vec<u8>>,
    unavailable: bool,
    reads: Arc<AtomicUsize>,
}

impl Keys {
    pub fn with(name: &str, value: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            values: BTreeMap::from([(name.to_owned(), value.to_vec())]),
            unavailable: false,
            reads: Arc::default(),
        })
    }

    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            values: BTreeMap::new(),
            unavailable: false,
            reads: Arc::default(),
        })
    }

    pub fn unavailable() -> Arc<Self> {
        Arc::new(Self {
            values: BTreeMap::new(),
            unavailable: true,
            reads: Arc::default(),
        })
    }

    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

struct Handle {
    values: BTreeMap<String, Vec<u8>>,
    unavailable: bool,
    reads: Arc<AtomicUsize>,
}

impl SecretStore for Keys {
    fn for_profile(&self, _: ProfileId) -> Port<'_, Box<dyn ProfileSecrets>> {
        let handle = Handle {
            values: self.values.clone(),
            unavailable: self.unavailable,
            reads: self.reads.clone(),
        };
        Box::pin(async move {
            if handle.unavailable {
                return Err(Error::Unavailable);
            }
            Ok(Box::new(handle) as Box<dyn ProfileSecrets>)
        })
    }
}

impl ProfileSecrets for Handle {
    fn resolve(&self, name: SecretName) -> Port<'_, SecretValue> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let found = self.values.get(name.as_str()).cloned();
        Box::pin(async move { found.map(SecretValue).ok_or(Error::Missing) })
    }
}

/// Captures `bot_*` tracing event fields on the current thread to verify keys and payloads never leak to logs.
#[derive(Clone, Default)]
pub struct Logs(Arc<std::sync::Mutex<Vec<String>>>);

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={value:?} ", field.name()));
    }
}

impl tracing::Subscriber for Logs {
    fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
        meta.target().starts_with("bot_")
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(fields.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

impl Logs {
    pub fn text(&self) -> String {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).join("\n")
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[derive(Clone, Copy)]
pub enum Gate {
    Open,
    Stale,
    Unreadable,
}

impl ActiveCheck for Gate {
    fn verify(&self) -> Port<'_, ()> {
        let verdict = match self {
            Gate::Open => Ok(()),
            Gate::Stale => Err(Error::Stale),
            Gate::Unreadable => Err(Error::Unavailable),
        };
        Box::pin(async move { verdict })
    }
}

/// Returns `Ok` before the key is read and `Stale` afterward to simulate activation change mid-flight.
pub struct RevokedByKeyRead(pub Arc<Keys>);

impl ActiveCheck for RevokedByKeyRead {
    fn verify(&self) -> Port<'_, ()> {
        let verdict = if self.0.reads() == 0 {
            Ok(())
        } else {
            Err(Error::Stale)
        };
        Box::pin(async move { verdict })
    }
}

pub fn endpoint(kind: ApiKind, api: &FakeApi) -> ApiEndpoint {
    let text = match kind {
        ApiKind::Anthropic => api.url(),
        ApiKind::OpenAi => format!("{}/v1", api.url()),
    };
    ApiEndpoint::parse(&text).expect("loopback endpoint")
}

pub fn path(kind: ApiKind) -> &'static str {
    match kind {
        ApiKind::Anthropic => "/v1/messages",
        ApiKind::OpenAi => "/v1/chat/completions",
    }
}

pub fn settings(kind: ApiKind, api: &FakeApi, secret: Option<&str>) -> ApiSettings {
    ApiSettings {
        kind,
        endpoint: endpoint(kind, api),
        model: "model-x".into(),
        secret: secret.and_then(SecretName::parse),
        auth: None,
        max_tokens: None,
        limit_field: Default::default(),
        effort: None,
    }
}

pub fn api_runner(kind: ApiKind, api: &FakeApi) -> ApiRunner {
    ApiRunner::new(
        settings(kind, api, Some("model-key")),
        Keys::with("model-key", KEY.as_bytes()),
        Arc::new(Gate::Open),
    )
    .expect("api runner")
}

pub const KINDS: [ApiKind; 2] = [ApiKind::Anthropic, ApiKind::OpenAi];

#[derive(Clone)]
pub struct Call {
    pub tool: &'static str,
    pub args: String,
    pub id: &'static str,
    pub signature: Option<&'static str>,
}

pub fn call(tool: &'static str, args: Value) -> Call {
    Call {
        tool,
        args: args.to_string(),
        id: "call_up",
        signature: None,
    }
}

fn usage_json(kind: ApiKind, usage: Option<(u64, u64)>) -> Value {
    match (kind, usage) {
        (_, None) => json!({}),
        (ApiKind::Anthropic, Some((input, output))) => {
            json!({"input_tokens": input, "output_tokens": output})
        }
        (ApiKind::OpenAi, Some((prompt, completion))) => {
            json!({"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion})
        }
    }
}

pub fn text(kind: ApiKind, text: &str, usage: Option<(u64, u64)>) -> Reply {
    let body = match kind {
        ApiKind::Anthropic => json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn", "usage": usage_json(kind, usage),
        }),
        ApiKind::OpenAi => json!({
            "id": "chatcmpl-1", "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
            "usage": usage_json(kind, usage),
        }),
    };
    Reply::Json(200, body.to_string())
}

pub fn calls(kind: ApiKind, text: &str, calls: Vec<Call>, usage: Option<(u64, u64)>) -> Reply {
    let text = text.to_owned();
    Reply::With(Arc::new(move |seen: &Seen| {
        let body = match kind {
            ApiKind::Anthropic => {
                let mut content = Vec::new();
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
                for call in &calls {
                    let input: Value = serde_json::from_str(&call.args).unwrap();
                    content.push(json!({"type": "tool_use", "id": call.id, "name": seen.alias(call.tool), "input": input}));
                }
                json!({
                    "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
                    "content": content, "stop_reason": "tool_use", "usage": usage_json(kind, usage),
                })
            }
            ApiKind::OpenAi => {
                let items: Vec<Value> = calls
                    .iter()
                    .map(|call| {
                        let mut item = json!({
                            "id": call.id, "type": "function",
                            "function": {"name": seen.alias(call.tool), "arguments": call.args},
                        });
                        if let Some(signature) = call.signature {
                            item["extra_content"] =
                                json!({"google": {"thought_signature": signature}});
                        }
                        item
                    })
                    .collect();
                json!({
                    "id": "chatcmpl-1", "object": "chat.completion",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": if text.is_empty() { Value::Null } else { Value::String(text.clone()) }, "tool_calls": items}, "finish_reason": "tool_calls"}],
                    "usage": usage_json(kind, usage),
                })
            }
        };
        Reply::Json(200, body.to_string())
    }))
}

pub fn cut(kind: ApiKind, with_call: bool) -> Reply {
    Reply::With(Arc::new(move |seen: &Seen| {
        let body = match kind {
            ApiKind::Anthropic => {
                let content = if with_call {
                    json!([{"type": "tool_use", "id": "t", "name": seen.alias("agenda.delete"), "input": {"event": "e"}}])
                } else {
                    json!([{"type": "text", "text": "partial answer"}])
                };
                json!({"type": "message", "content": content, "stop_reason": "max_tokens", "usage": {"input_tokens": 5, "output_tokens": 4096}})
            }
            ApiKind::OpenAi => {
                let message = if with_call {
                    json!({"content": null, "tool_calls": [{"id": "t", "type": "function", "function": {"name": seen.alias("agenda.delete"), "arguments": "{\"event\":\"e"}}]})
                } else {
                    json!({"content": "partial answer"})
                };
                json!({"choices": [{"message": message, "finish_reason": "length"}], "usage": {"prompt_tokens": 5, "completion_tokens": 4096}})
            }
        };
        Reply::Json(200, body.to_string())
    }))
}
