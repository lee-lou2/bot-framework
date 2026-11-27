//! Shared request-building and strict response-parsing helpers for Anthropic and OpenAI-compatible wire dialects.

mod anthropic;
mod openai;

use super::{names::NameTable, runner::ApiSettings, transport::Header};
use crate::model::{
    CallEnd, ModelError, ModelRequest, ModelStep, Role, ToolSpec, Usage, prompt::clip,
};
use bot_core::command::{JsonPayload, TokenUsage};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

pub(crate) const MAX_CALLS: usize = 32;
pub(crate) const MAX_ARGS_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiKind {
    /// `POST {endpoint}/v1/messages` (Anthropic Messages).
    Anthropic,
    /// `POST {endpoint}/chat/completions` (OpenAI-compatible; `endpoint` includes `/v1`).
    OpenAi,
}

pub(crate) struct Built {
    pub body: Vec<u8>,
    pub names: NameTable,
}

impl ApiKind {
    pub(crate) fn path(self) -> &'static str {
        match self {
            ApiKind::Anthropic => anthropic::PATH,
            ApiKind::OpenAi => openai::PATH,
        }
    }

    pub(crate) fn headers(self, key: Option<&str>) -> Vec<Header> {
        match self {
            ApiKind::Anthropic => anthropic::headers(key),
            ApiKind::OpenAi => openai::headers(key),
        }
    }

    pub(crate) fn build(
        self,
        settings: &ApiSettings,
        request: &ModelRequest,
    ) -> Result<Built, ModelError> {
        match self {
            ApiKind::Anthropic => anthropic::build(settings, request),
            ApiKind::OpenAi => openai::build(settings, request),
        }
    }

    /// Rejects ambiguous JSON (e.g. duplicate keys) before reading any fields so parsers cannot disagree.
    pub(crate) fn parse(
        self,
        settings: &ApiSettings,
        body: &[u8],
        request: &ModelRequest,
        names: &NameTable,
    ) -> Result<ModelStep, ModelError> {
        let text = std::str::from_utf8(body).map_err(|_| invalid())?;
        let value: Value = serde_json::from_str(text).map_err(|_| invalid())?;
        if bot_core::json::parse(text).is_none() {
            return Err(if self.has_tool_calls(&value) {
                fail("invalid_tool_args", false)
            } else {
                invalid()
            });
        }
        match self {
            ApiKind::Anthropic => anthropic::parse(settings, &value, request, names),
            ApiKind::OpenAi => openai::parse(settings, &value, request, names),
        }
    }

    fn has_tool_calls(self, value: &Value) -> bool {
        match self {
            ApiKind::Anthropic => {
                value
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks
                            .iter()
                            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    })
            }
            ApiKind::OpenAi => value
                .pointer("/choices/0/message/tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty()),
        }
    }
}

pub(crate) fn fail(code: &'static str, transient: bool) -> ModelError {
    ModelError::Failed { code, transient }
}

pub(crate) fn invalid() -> ModelError {
    fail("invalid_response", false)
}

pub(crate) fn name_table(request: &ModelRequest) -> NameTable {
    NameTable::new(
        request.tools.iter().map(|tool| &tool.id).chain(
            request
                .exchanges
                .iter()
                .flat_map(|exchange| exchange.calls.iter().map(|call| &call.tool)),
        ),
    )
}

pub(crate) struct Turn {
    pub role: Role,
    pub text: String,
}

pub(crate) fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// Normalizes conversation turns: drops blanks, merges consecutive same-role turns, strips leading assistant turns, and prefixes `[speaker]` when multiple users appear.
pub(crate) fn conversation(request: &ModelRequest) -> Vec<Turn> {
    let speakers: BTreeSet<&str> = request
        .conversation
        .iter()
        .filter(|message| message.role == Role::User)
        .filter_map(|message| message.speaker.as_ref().map(|who| who.0.as_str()))
        .collect();
    let label = speakers.len() > 1;
    let mut turns: Vec<Turn> = Vec::new();
    for message in &request.conversation {
        let text = message.text.trim();
        if text.is_empty() {
            continue;
        }
        let text = match (&message.speaker, label, message.role) {
            (Some(who), true, Role::User) => format!("[{}] {text}", who.0),
            _ => text.to_owned(),
        };
        match turns.last_mut() {
            Some(last) if last.role == message.role => {
                last.text.push_str("\n\n");
                last.text.push_str(&text);
            }
            _ => turns.push(Turn {
                role: message.role,
                text,
            }),
        }
    }
    while turns
        .first()
        .is_some_and(|turn| turn.role == Role::Assistant)
    {
        turns.remove(0);
    }
    turns
}

pub(crate) fn result_text(end: &CallEnd) -> (String, bool) {
    match end {
        CallEnd::Done(result) => (clip(&result.0), false),
        CallEnd::Failed(code) if bot_core::cli::CliRefusal::from_word(code).is_some() => {
            (code.clone(), true)
        }
        CallEnd::Failed(code) => (format!("failed: {code}"), true),
        CallEnd::Denied => ("Not approved or blocked; not executed.".to_owned(), true),
    }
}

pub(crate) fn args_object(payload: &JsonPayload) -> Value {
    match serde_json::from_str::<Value>(&payload.0) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({}),
    }
}

/// Ensures the tool schema is a JSON object with `type: "object"` as required by upstream APIs.
pub(crate) fn schema_of(tool: &ToolSpec) -> Value {
    let mut schema = match serde_json::from_str::<Value>(&tool.input.0) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    schema
        .entry("type")
        .or_insert_with(|| Value::String("object".to_owned()));
    Value::Object(schema)
}

/// Validates that model-emitted call arguments are a JSON object within `MAX_ARGS_BYTES` (never defaults missing/null args).
pub(crate) fn checked_args(args: Value) -> Result<String, ModelError> {
    match args {
        Value::Object(_) => {
            let text = args.to_string();
            if text.len() > MAX_ARGS_BYTES {
                Err(fail("invalid_tool_args", false))
            } else {
                Ok(text)
            }
        }
        _ => Err(fail("invalid_tool_args", false)),
    }
}

/// Only emits `measured` when both prompt and completion counts are positive (never fabricates zeroes).
pub(crate) fn token_usage(
    prompt: Option<u64>,
    completion: Option<u64>,
) -> (Usage, Option<TokenUsage>) {
    let usage = Usage {
        input_tokens: prompt.unwrap_or(0),
        output_tokens: completion.unwrap_or(0),
    };
    let measured = match (prompt, completion) {
        (Some(prompt), Some(completion)) if prompt > 0 && completion > 0 => {
            Some(TokenUsage { prompt, completion })
        }
        _ => None,
    };
    (usage, measured)
}
