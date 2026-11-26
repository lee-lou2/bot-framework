//! 상류 API 두 가지(Anthropic Messages · OpenAI 호환 Chat Completions)의 요청 만들기와 응답 읽기. 두 형식이 같이 쓰는 규칙은 여기에 둔다.
//!
//! 요청은 맥락 조립(`context::assemble`)의 결과만 옮긴다: 시스템 글은 시스템 자리에, 대화(사용자·비서 줄)는 대화 자리에, 이번 run의 이전 차례(`Exchange`)는 «비서의 도구 호출 →
//! 도구 결과» 쌍으로 이어 붙인다. 도구 결과는 모델이 만든 글이 아니라 비신뢰 입력이라 사용자 역할 자리(`tool_result`·`tool`)에만 간다. 응답은 모양을 엄격하게 읽는다:
//! 깨졌거나 모르는 종료 사유·도구 이름·인자는 도구를 하나도 부르지 않고 고정 코드로 끝낸다.

mod anthropic;
mod openai;

use super::{names::NameTable, runner::ApiSettings, transport::Header};
use crate::model::{
    CallEnd, ModelError, ModelRequest, ModelStep, Role, ToolSpec, Usage, prompt::clip,
};
use bot_core::command::{JsonPayload, TokenUsage};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// 한 차례에 모델이 부를 수 있는 호출 수 상한(넘으면 하나도 부르지 않는다)
pub(crate) const MAX_CALLS: usize = 32;
/// 호출 하나의 인자 글 크기 상한
pub(crate) const MAX_ARGS_BYTES: usize = 256 * 1024;

/// 상류 형식
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiKind {
    /// `POST {endpoint}/v1/messages`: Anthropic Messages(tool use)
    Anthropic,
    /// `POST {endpoint}/chat/completions`: OpenAI 호환 Chat Completions(tool_calls). `endpoint`는 `/v1`까지 포함한 기준 주소다.
    OpenAi,
}

/// 만든 요청: 본문과, 응답의 도구 이름을 되돌릴 별칭표
pub(crate) struct Built {
    pub body: Vec<u8>,
    pub names: NameTable,
}

impl ApiKind {
    /// 기준 주소 뒤에 붙이는 요청 경로
    pub(crate) fn path(self) -> &'static str {
        match self {
            ApiKind::Anthropic => anthropic::PATH,
            ApiKind::OpenAi => openai::PATH,
        }
    }

    /// 요청 머리. 키는 있을 때만 싣고 비밀로 표시한다.
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

    /// 응답 본문을 읽는다. 같은 키가 두 번 나오는 등 모호한 글은 파서마다 읽는 값이 갈리므로 어느 값도 읽기 전에 거절한다(호출이 있는 응답이면 `invalid_tool_args`, 아니면
    /// `invalid_response`).
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
            ApiKind::Anthropic => anthropic::parse(&value, request, names),
            ApiKind::OpenAi => openai::parse(settings, &value, request, names),
        }
    }

    /// 응답에 도구 호출이 들어 있는가(모양을 믿지 않고 눈으로만 훑는다)
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

/// 응답 모양이 기대와 다르다(영구 실패: 다시 해도 같은 상류는 같은 모양을 준다)
pub(crate) fn invalid() -> ModelError {
    fail("invalid_response", false)
}

/// 이 요청에서 모델에게 보이거나 지난 차례에 나온 모든 도구 ID로 만든 별칭표
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

/// 상류 형식의 대화 한 줄
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

/// 대화(`ModelRequest::conversation`)를 상류가 받는 모양으로 다듬는다: 빈 줄은 버리고, 같은 역할이 이어지면 한 줄로 합치고(엄격한 번갈아 말하기를 요구하는 서버도 받는다),
/// 사용자로 시작하도록 앞의 비서 줄은 뗀다. 발화자가 둘 이상이면 사용자 줄 앞에 `[발화자]`를 붙인다(맥락 조립이 정한 줄 그대로이고 시스템 자리에는 아무것도 더하지 않는다).
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

/// 도구 호출의 결말을 모델에게 보이는 글로: (글, 실패인가). 결과는 CLI 실행기와 같은 한도로 줄인다.
pub(crate) fn result_text(end: &CallEnd) -> (String, bool) {
    match end {
        CallEnd::Done(result) => (clip(&result.0), false),
        CallEnd::Failed(code) if bot_core::cli::CliRefusal::from_word(code).is_some() => {
            (code.clone(), true)
        }
        CallEnd::Failed(code) => (format!("실패: {code}"), true),
        CallEnd::Denied => (
            "승인되지 않았거나 막혀서 실행하지 않았어요".to_owned(),
            true,
        ),
    }
}

/// 지난 차례에 모델이 낸 인자(JSON 객체). 객체가 아니면 빈 객체다(워커는 객체만 계획에 적었다).
pub(crate) fn args_object(payload: &JsonPayload) -> Value {
    match serde_json::from_str::<Value>(&payload.0) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({}),
    }
}

/// 도구의 입력 스키마(JSON 객체). 객체가 아니거나 `type`이 없으면 `type: object`를 보충한다(상류가 요구한다).
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

/// 모델이 낸 호출 인자를 계획에 적을 글로: JSON 객체여야 하고 크기 상한을 지킨다. 없는 인자(`null`·누락)를 빈 객체로 짐작하지 않는다.
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

/// 공급자가 보고한 토큰 수에서 (관측용, 실측). 실측은 입력·출력이 모두 보고되고 양수일 때만이다(실제 요청은 입력도 출력도 0일 수 없다 — 0으로 채운 보고를 실측으로 쓰지 않는다).
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
