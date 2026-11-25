//! 성공 응답의 모양: chat.completion 본문, SSE 청크, 모델 객체, 고정 안내 문구. 고정한 공식 OpenAPI의 응답 스키마를 따른다.
//!
//! 사용량(`usage`)은 실제 전체 실행의 신뢰할 수 있는 공급자 계수가 있을 때만 싣는다. 고정 안내·모름·추정치·0 채우기는 측정값이 아니다.
use bot_core::{api::ApprovalBlock, command::TokenUsage};
use serde_json::{Map, Value, json};

/// 모델 객체의 소유자 표시
pub const OWNED_BY: &str = "bot-framework";
pub const KEEP_ALIVE: &str = ": keep-alive\n\n";
pub const DONE: &str = "data: [DONE]\n\n";
/// SSE 한 청크에 싣는 글의 최대 바이트(UTF-8 문자 경계에서 자른다)
const DELTA_BYTES: usize = 4096;

const RETRY_HINT: &str = "같은 Idempotency-Key와 같은 요청으로 다시 조회하세요.";

/// 고정 서비스 안내. 승인 대기·진행 중을 꾸미거나 모델의 말로 바꾸지 않는다.
pub fn approval_pending() -> String {
    format!("승인 대기 중입니다. {RETRY_HINT}")
}

pub fn still_running() -> String {
    format!("작업을 처리 중입니다. {RETRY_HINT}")
}

/// 사람 승인을 받을 수 없어 도구가 실행되지 않았다는 호스트 판정 안내. 모델의 글이 아니라 호스트가 정한 문구다.
pub fn blocked(reason: ApprovalBlock) -> String {
    let why = match reason {
        ApprovalBlock::NoApprover => "승인할 사람이 지정되지 않았어요",
        ApprovalBlock::ApproverRevoked => "지정된 승인자가 더는 유효하지 않아요",
        ApprovalBlock::RouteChanged => "승인 경로가 바뀌었어요",
        ApprovalBlock::ApprovalDmUnavailable => "승인 요청을 전달하지 못했어요",
    };
    format!("{why}. 승인이 필요한 작업은 실행하지 않았습니다.")
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

/// 한 실행의 응답 식별: 모든 청크·재조회에서 같다.
#[derive(Clone, Debug)]
pub struct Identity {
    pub id: String,
    /// 최초 접수 시각(DB 시계)의 Unix 초
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

/// 스트리밍이 아닌 성공 본문(`chat.completion`)
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

/// SSE 청크 하나. `usage_null`이면 일반 청크에 `usage: null`을 싣는다(`include_usage` 요청일 때만).
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

/// `data: {한 줄 JSON}` + 빈 줄
pub fn event(value: &Value) -> String {
    format!("data: {value}\n\n")
}

/// 첫 청크: 역할만 알린다.
pub fn first_chunk(identity: &Identity, include_usage: bool) -> String {
    chunk(
        identity,
        json!([{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]),
        include_usage.then_some(Value::Null),
    )
}

/// 최종 글(또는 안내)을 UTF-8 문자를 깨지 않는 content delta 청크들로 나눈다.
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

/// 마지막 선택지: 빈 delta와 종료 사유
pub fn finish_chunk(identity: &Identity, finish: Finish, include_usage: bool) -> String {
    chunk(
        identity,
        json!([{"index": 0, "delta": {}, "finish_reason": finish.name()}]),
        include_usage.then_some(Value::Null),
    )
}

/// 측정한 사용량만 싣는 마지막 청크(`choices: []`)
pub fn usage_chunk(identity: &Identity, usage: TokenUsage) -> String {
    chunk(identity, json!([]), Some(usage_value(usage)))
}

/// 글을 `max` 바이트 이하의 조각으로 나눈다. 조각은 항상 UTF-8 문자 경계에서 끊는다.
pub fn pieces(text: &str, max: usize) -> Vec<&str> {
    // 한 문자는 최대 4바이트다. 더 작게 주면 문자 하나도 담지 못해 끝나지 않는다.
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

/// `GET /v1/models`의 모델 객체
pub fn model(id: &str, created: u64) -> Value {
    json!({"id": id, "object": "model", "created": created, "owned_by": OWNED_BY})
}

pub fn model_list(models: Vec<Value>) -> Value {
    json!({"object": "list", "data": models})
}

#[cfg(test)]
mod tests;
