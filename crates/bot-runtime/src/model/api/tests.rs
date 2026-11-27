//! Pure unit tests for direct API runner endpoints, tool aliasing, request building, and response parsing.

mod anthropic;
mod endpoint;
mod names;
mod openai;
mod strict;

use super::{ApiEndpoint, ApiKind, ApiSettings};
use crate::model::{
    CallEnd, DoneCall, Exchange, Limits, Message, ModelRequest, Role, ToolKind, ToolSpec,
};
use bot_core::{
    command::JsonPayload,
    id::{
        Attempt, CallId, ClaimEpoch, Generation, PrincipalId, ProfileId, RunId, SecretName, ToolId,
    },
    state::Risk,
};
use serde_json::Value;
use std::collections::BTreeSet;
use tokio::sync::watch;

pub(super) fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        speaker: Some(PrincipalId("local:owner".into())),
        text: text.into(),
    }
}

pub(super) fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        speaker: None,
        text: text.into(),
    }
}

pub(super) fn tool(id: &str, description: &str, schema: &str) -> ToolSpec {
    ToolSpec {
        id: ToolId(id.into()),
        kind: ToolKind::Skill,
        class: Risk::Read,
        description: description.into(),
        input: JsonPayload(schema.into()),
    }
}

pub(super) fn request(
    conversation: Vec<Message>,
    tools: Vec<ToolSpec>,
    exchanges: Vec<Exchange>,
) -> ModelRequest {
    ModelRequest {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("run-1".into()),
            generation: Generation(1),
            claim: Some(ClaimEpoch(1)),
        },
        model: "assistant".into(),
        system: "You are a schedule assistant. Summarize the user's schedule concisely.".into(),
        conversation,
        tools,
        exchanges,
        limits: Limits::default(),
        cancel: watch::channel(false).1,
        builtin: BTreeSet::new(),
        relay: None,
        session: None,
    }
}

pub(super) fn settings(kind: ApiKind) -> ApiSettings {
    ApiSettings {
        kind,
        endpoint: ApiEndpoint::parse("https://api.example.com/v1").unwrap(),
        model: "model-x".into(),
        secret: SecretName::parse("model-key"),
        max_tokens: None,
        limit_field: Default::default(),
        effort: None,
    }
}

pub(super) fn done_call(id: &str, tool: &str, args: &str, end: CallEnd) -> DoneCall {
    DoneCall {
        call: CallId(id.into()),
        tool: ToolId(tool.into()),
        args: JsonPayload(args.into()),
        end,
    }
}

pub(super) fn exchange(text: &str, calls: Vec<DoneCall>, state: Option<&str>) -> Exchange {
    Exchange {
        text: text.into(),
        calls,
        state: state.map(str::to_owned),
    }
}

pub(super) fn body_of(kind: ApiKind, request: &ModelRequest) -> Value {
    let built = kind
        .build(&settings(kind), request)
        .unwrap_or_else(|e| panic!("failed to build request: {e:?}"));
    serde_json::from_slice(&built.body).unwrap()
}
