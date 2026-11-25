//! API 직접 호출 실행기의 순수 부분 시험(주소 규칙·도구 이름 별칭표·요청 만들기·응답 읽기). 실제 HTTP 전송과 워커·브로커를 거치는 시험은 `tests/api_*.rs`다.

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

/// 읽기 도구 하나(입력 스키마 지정)
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
        system: "너는 일정 비서다. 사용자의 일정을 한국어로 정리한다.".into(),
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

/// 만든 요청 본문을 JSON으로 읽는다.
pub(super) fn body_of(kind: ApiKind, request: &ModelRequest) -> Value {
    let built = kind
        .build(&settings(kind), request)
        .unwrap_or_else(|e| panic!("요청을 만들지 못했어요: {e:?}"));
    serde_json::from_slice(&built.body).unwrap()
}
