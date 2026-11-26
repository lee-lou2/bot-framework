//! 열 값과 core 타입 사이의 변환, 입력 검증, 정규화·해시.

use crate::db::R;
use bot_core::{
    command::{JsonPayload, Target},
    id::*,
    port::Error,
    state::*,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// 한 입력 문서의 상한. 과대 입력은 접수하지 않는다.
pub(crate) const MAX_DOC_BYTES: usize = 1 << 20;
const MAX_ID_BYTES: usize = 256;
const MAX_ATTACHMENTS: usize = 64;

pub(crate) fn invalid<T>() -> R<T> {
    Err(Error::Invalid.into())
}

/// 식별자 문자열: 비어 있지 않고 길이가 제한되며 제어 문자(NUL 포함)가 없다.
pub(crate) fn id_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ID_BYTES && !s.chars().any(char::is_control)
}

pub(crate) fn ids_ok(items: &[&str]) -> R<()> {
    if items.iter().all(|s| id_ok(s)) {
        Ok(())
    } else {
        invalid()
    }
}

pub(crate) fn attachments_ok(items: &[String]) -> R<()> {
    if items.len() <= MAX_ATTACHMENTS
        && items
            .iter()
            .all(|s| id_ok(s) || s.len() <= 1024 && !s.is_empty())
    {
        Ok(())
    } else {
        invalid()
    }
}

pub(crate) fn kind_str(kind: &ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Direct => "direct",
        ConversationKind::Channel => "channel",
        ConversationKind::Cli => "cli",
        ConversationKind::Http => "http",
        ConversationKind::Scheduled => "scheduled",
    }
}

pub(crate) fn kind_from(s: &str) -> R<ConversationKind> {
    Ok(match s {
        "direct" => ConversationKind::Direct,
        "channel" => ConversationKind::Channel,
        "cli" => ConversationKind::Cli,
        "http" => ConversationKind::Http,
        "scheduled" => ConversationKind::Scheduled,
        _ => return invalid(),
    })
}

pub(crate) fn state_text(s: RunState) -> &'static str {
    match s {
        RunState::Ready => "ready",
        RunState::Leased => "leased",
        RunState::Waiting => "waiting",
        RunState::Retry => "retry",
        RunState::Succeeded => "succeeded",
        RunState::Dead => "dead",
        RunState::Cancelled => "cancelled",
    }
}

pub(crate) fn state_from(s: &str) -> R<RunState> {
    Ok(match s {
        "ready" => RunState::Ready,
        "leased" => RunState::Leased,
        "waiting" => RunState::Waiting,
        "retry" => RunState::Retry,
        "succeeded" => RunState::Succeeded,
        "dead" => RunState::Dead,
        "cancelled" => RunState::Cancelled,
        _ => return invalid(),
    })
}

/// 검증한 JSON 문서를 키 순서가 고정된 문자열로 다시 쓴다(공백·키 순서가 달라도 같은 값).
pub(crate) fn canonical(payload: &JsonPayload) -> R<(Value, String)> {
    if payload.0.len() > MAX_DOC_BYTES {
        return invalid();
    }
    let value: Value = serde_json::from_str(&payload.0).map_err(|_| Error::Invalid)?;
    let text = serde_json::to_string(&value).map_err(|_| Error::Invalid)?;
    Ok((value, text))
}

pub(crate) fn target_json(target: &Target) -> R<Value> {
    Ok(match target {
        Target::Agent { agent, instruction } => {
            if instruction.len() > MAX_DOC_BYTES {
                return invalid();
            }
            json!({"kind": "agent", "agent": agent.0, "instruction": instruction})
        }
        Target::Tool { agent, tool, args } => {
            let (args, _) = canonical(args)?;
            if !args.is_object() {
                return invalid();
            }
            json!({"kind": "tool", "agent": agent.0, "tool": tool.0, "args": args})
        }
    })
}

pub(crate) fn target_from(value: &Value) -> R<Target> {
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    let agent = AgentId(field("agent").ok_or(Error::Invalid)?);
    match value.get("kind").and_then(Value::as_str) {
        Some("agent") => Ok(Target::Agent {
            agent,
            instruction: field("instruction").ok_or(Error::Invalid)?,
        }),
        Some("tool") => {
            let args = value
                .get("args")
                .filter(|args| args.is_object())
                .ok_or(Error::Invalid)?;
            Ok(Target::Tool {
                agent,
                tool: ToolId(field("tool").ok_or(Error::Invalid)?),
                args: JsonPayload(serde_json::to_string(args).map_err(|_| Error::Invalid)?),
            })
        }
        _ => invalid(),
    }
}

pub(crate) fn agent_of(target: &Target) -> &AgentId {
    match target {
        Target::Agent { agent, .. } | Target::Tool { agent, .. } => agent,
    }
}

/// 32바이트 해시 열을 배열로 바꾼다(길이가 다르면 `Invalid`).
pub(crate) fn to32(bytes: Vec<u8>) -> R<[u8; 32]> {
    bytes.try_into().map_err(|_| Error::Invalid.into())
}

pub(crate) fn sha256_text(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

/// 대화 문서에서 사람이 읽을 글만 꺼낸다: `{"text": "…"}`면 그 글, 아니면 문서 그대로.
pub(crate) fn text_of(doc: &str) -> String {
    match serde_json::from_str::<Value>(doc) {
        Ok(Value::Object(map)) => match map.get("text") {
            Some(Value::String(text)) => text.clone(),
            _ => doc.to_owned(),
        },
        Ok(Value::String(text)) => text,
        _ => doc.to_owned(),
    }
}
