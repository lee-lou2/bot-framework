//! 모델 샌드박스의 단일 IPC 프레임 처리기. 모델 샌드박스는 비밀·네트워크가 없고 이 채널 하나로만 바깥과 말한다.
//!
//! UTF-8 NDJSON 한 줄이 요청 또는 응답 하나다. 요청은 두 가지뿐이다.
//! - `{"v":1,"id":"1","op":"model","body":{…}}` → `{"v":1,"id":"1","status":200,"body":{…}}`
//!   (모델 별칭은 신뢰 호스트가 세션에 고정한다. 샌드박스가 별칭을 고를 수 없다.)
//! - `{"v":1,"id":"2","op":"tool","tool":"agenda.list","args":{…}}` →
//!   `{"v":1,"id":"2","state":"done","result":{…}}` / `"awaiting"` / `"held"` / `{"error":"denied"}`
//!
//! 세션은 호스트가 만든 `attempt`(프로필·run·세대·claim)에 묶인다. 샌드박스는 호출 ID·run·세대·주체·승인자를
//! 지정할 수 없다(그런 키가 있으면 프레임 전체를 거절한다). 호출 ID는 호스트가 준 발급기가 정한다. 이 채널에는 승인
//! 결정이 없다: 모델은 승인할 수 없고 `awaiting`에는 nonce도 싣지 않는다. 각 메시지는 브로커가 run·세대·claim을 다시 본다.

use crate::{
    canon::canonicalize,
    json::parse,
    lines::read_line,
    model::*,
    sandbox::{BoxRead, BoxWrite},
    text::json_quote,
};
use bot_core::{
    command::{CallResult, JsonPayload, ToolCall},
    id::*,
    port::{Broker, Error},
};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    sync::watch,
};

/// 프레임 한 줄의 상한(모델 요청 본문 4 MiB + 여유).
pub const MAX_FRAME: usize = 5 << 20;

/// 호출 ID 발급기. 재개해도 같은 호출이 같은 ID를 받아야 하므로 runtime은 체크포인트에 맞춘 구현을 줄 수 있다.
pub trait CallIds: Send + Sync {
    fn next(&self) -> CallId;
}

/// `c-1`, `c-2`처럼 세션 안에서 차례로 매긴다.
pub struct SequentialIds {
    prefix: String,
    next: Mutex<u64>,
}

impl SequentialIds {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.into(),
            next: Mutex::new(1),
        }
    }
}

impl CallIds for SequentialIds {
    fn next(&self) -> CallId {
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let id = CallId(format!("{}-{}", self.prefix, *next));
        *next += 1;
        id
    }
}

pub struct ModelSession {
    broker: Arc<dyn Broker>,
    model: Arc<ModelProxy>,
    attempt: Attempt,
    /// 실행 에이전트의 모델 별칭. 신뢰 호스트가 정한다.
    alias: String,
    ids: Arc<dyn CallIds>,
}

fn error_name(error: &Error) -> &'static str {
    match error {
        Error::Denied => "denied",
        Error::Conflict => "conflict",
        Error::Stale => "stale",
        Error::Unavailable => "unavailable",
        Error::Invalid | Error::Missing => "invalid",
    }
}

fn failure(id: Option<&str>, class: &str) -> String {
    match id {
        Some(id) => format!(
            "{{\"v\":1,\"id\":{},\"error\":{}}}",
            json_quote(id),
            json_quote(class)
        ),
        None => format!("{{\"v\":1,\"error\":{}}}", json_quote(class)),
    }
}

/// 프레임에 허용한 키만 있는지 본다. 이 밖의 키(호출 ID·run·세대·주체 등)는 샌드박스가 정할 수 없다.
fn only_keys(frame: &Map<String, Value>, allowed: &[&str]) -> bool {
    frame.keys().all(|key| allowed.contains(&key.as_str()))
}

impl ModelSession {
    pub fn new(
        broker: Arc<dyn Broker>,
        model: Arc<ModelProxy>,
        attempt: Attempt,
        alias: String,
        ids: Arc<dyn CallIds>,
    ) -> Self {
        Self {
            broker,
            model,
            attempt,
            alias,
            ids,
        }
    }

    /// 프레임 한 줄을 처리하고 응답 한 줄(개행 없음)을 돌려준다.
    pub async fn handle(&self, line: &[u8]) -> String {
        let Some(Value::Object(frame)) = std::str::from_utf8(line).ok().and_then(parse) else {
            return failure(None, "invalid");
        };
        let id = match frame.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() && id.len() <= 64 => id,
            _ => return failure(None, "invalid"),
        };
        if frame.get("v").and_then(Value::as_u64) != Some(1) {
            return failure(Some(id), "invalid");
        }
        let result = match frame.get("op").and_then(Value::as_str) {
            Some("model") if only_keys(&frame, &["v", "id", "op", "body"]) => {
                self.model_op(id, &frame).await
            }
            Some("tool") if only_keys(&frame, &["v", "id", "op", "tool", "args"]) => {
                self.tool_op(id, &frame).await
            }
            _ => Err(Error::Invalid),
        };
        result.unwrap_or_else(|error| failure(Some(id), error_name(&error)))
    }

    async fn model_op(&self, id: &str, frame: &Map<String, Value>) -> Result<String, Error> {
        let body = frame
            .get("body")
            .filter(|b| b.is_object())
            .ok_or(Error::Invalid)?;
        let body = JsonPayload(canonicalize(body).ok_or(Error::Invalid)?);
        let reply = self
            .model
            .forward(ModelRequest {
                attempt: self.attempt.clone(),
                alias: self.alias.clone(),
                body,
                headers: Vec::new(),
            })
            .await?;
        Ok(format!(
            "{{\"v\":1,\"id\":{},\"status\":{},\"body\":{}}}",
            json_quote(id),
            reply.status,
            reply.body.0
        ))
    }

    async fn tool_op(&self, id: &str, frame: &Map<String, Value>) -> Result<String, Error> {
        let tool = frame
            .get("tool")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && t.len() <= 127)
            .ok_or(Error::Invalid)?;
        let args = frame
            .get("args")
            .filter(|a| a.is_object())
            .ok_or(Error::Invalid)?;
        let args = JsonPayload(canonicalize(args).ok_or(Error::Invalid)?);
        let result = self
            .broker
            .call(ToolCall {
                attempt: self.attempt.clone(),
                call: self.ids.next(),
                tool: ToolId(tool.to_owned()),
                args,
            })
            .await?;
        let id = json_quote(id);
        Ok(match result {
            CallResult::Done(payload) => format!(
                "{{\"v\":1,\"id\":{id},\"state\":\"done\",\"result\":{}}}",
                payload.0
            ),
            // 승인 nonce는 모델에 주지 않는다. 승인은 승인자가 다른 길로 한다.
            CallResult::Awaiting(_) => format!("{{\"v\":1,\"id\":{id},\"state\":\"awaiting\"}}"),
            CallResult::Held(_) => format!("{{\"v\":1,\"id\":{id},\"state\":\"held\"}}"),
            // 넘김은 호스트가 직접 시작한 호출에서만 일어난다 — 도구의 효과 채널에는 오지 않는다
            CallResult::Passed => return Err(Error::Unavailable),
        })
    }

    /// `stop`이 오거나 연결이 끝날 때까지 프레임을 처리한다. 처리 중인 프레임은 끝까지 마친다.
    pub async fn serve(self, read: BoxRead, mut write: BoxWrite, mut stop: watch::Receiver<bool>) {
        let mut read = BufReader::new(read);
        loop {
            let line = tokio::select! {
                biased;
                _ = stop.changed() => return,
                line = read_line(&mut read, MAX_FRAME) => line,
            };
            let reply = match line {
                Ok(Some(line)) => self.handle(&line).await,
                Ok(None) => return,
                Err(_) => {
                    let _ = write
                        .write_all(format!("{}\n", failure(None, "invalid")).as_bytes())
                        .await;
                    return;
                }
            };
            if write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .is_err()
                || write.flush().await.is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;
