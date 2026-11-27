//! Shared `ModelRequest` builder and single-step helper for API runner tests.

use bot_core::{
    command::JsonPayload,
    id::{Attempt, ClaimEpoch, Generation, PrincipalId, ProfileId, RunId, ToolId},
    state::Risk,
};
use bot_runtime::model::{
    ApiRunner, Limits, Message, ModelError, ModelRequest, ModelRunner, ModelStep, Role, ToolKind,
    ToolSpec,
};
use tokio::sync::watch;

pub fn tools() -> Vec<ToolSpec> {
    ["agenda.list", "agenda.delete"]
        .map(|id| ToolSpec {
            id: ToolId(id.into()),
            kind: ToolKind::Skill,
            class: Risk::Read,
            description: format!("{id} tool"),
            input: JsonPayload(r#"{"type":"object"}"#.into()),
        })
        .into()
}

pub fn request_with(cancel: watch::Receiver<bool>, limits: Limits) -> ModelRequest {
    ModelRequest {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("run-1".into()),
            generation: Generation(1),
            claim: Some(ClaimEpoch(1)),
        },
        model: "assistant".into(),
        system: "profile instructions".into(),
        conversation: vec![Message {
            role: Role::User,
            speaker: Some(PrincipalId("local:owner".into())),
            text: "Show today's agenda".into(),
        }],
        tools: tools(),
        exchanges: Vec::new(),
        limits,
        cancel,
        builtin: Default::default(),
        relay: None,
        session: None,
    }
}

pub fn request() -> ModelRequest {
    request_with(watch::channel(false).1, Limits::default())
}

pub fn failed(code: &'static str, transient: bool) -> ModelError {
    ModelError::Failed { code, transient }
}

pub async fn step(runner: &ApiRunner, request: ModelRequest) -> Result<ModelStep, ModelError> {
    runner.step(request).await
}
