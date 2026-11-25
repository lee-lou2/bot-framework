//! 의미 지문: 같은 멱등 키로 다시 온 요청이 «같은 일»인지 가르는 SHA-256.
//!
//! 정규 JSON은 모델 이름·검증한 메시지(순서·역할·내용·지원하는 `name`)·**실제로 쓰는** 실행 옵션이다. 지금은 쓰는 옵션이 없다(호스트가 모델 설정을 고정한다).
//! `stream`·`stream_options`와 받되 쓰지 않는 필드(temperature·metadata 등)는 표현·무시 값이라 넣지 않는다 — 같은 일을 스트림으로 다시 받아도 같은 작업에 붙는다.
use bot_core::api::ChatBody;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// 지문 정규형의 판. 정규형이 바뀌면 올린다(옛 지문과 섞이지 않는다).
const VERSION: u32 = 1;

pub fn semantic(model: &str, body: &ChatBody) -> [u8; 32] {
    let messages: Vec<Value> = body
        .turns
        .iter()
        .map(|turn| {
            let mut message = json!({"role": turn.role.label(), "content": turn.content});
            if let Some(name) = &turn.name {
                message["name"] = Value::String(name.clone());
            }
            message
        })
        .collect();
    let canonical = json!({
        "version": VERSION,
        "model": model,
        "messages": messages,
        "options": {},
    });
    Sha256::digest(canonical.to_string().as_bytes()).into()
}

#[cfg(test)]
mod tests;
