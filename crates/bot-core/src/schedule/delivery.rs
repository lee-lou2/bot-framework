//! 명시 목적지 예약의 최종 회신 묶음(`schedule.deliver`): 호출 상수와 승인 카드에 보이는 초안 문서, 수신 경로의 봉인.

use super::{DeliveryDraft, DeliveryTarget};
use crate::{canon, command::JsonPayload, id::is_name, port::Error, state::ConversationKind};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

/// 한 예약 실행에서 최종 회신 묶음은 하나다. 이 호출 ID는 호스트가 소유한다.
pub const DELIVER_CALL: &str = "schedule-deliver";
pub const DELIVER_TOOL: &str = "schedule.deliver";
pub const DELIVER_REVISION: &str = "builtin-1";
pub const DELIVERY_CARD_LABEL: &str = "전송 초안";

// 게이트웨이 카드 전체가 20줄, 줄마다 500바이트다. 실제 렌더러는 머리줄도 함께 센다.
const CARD_CAPACITY: usize = 20 * 500;

impl DeliveryTarget {
    /// 예약 생성과 실제 발송에 같은 방식으로 수신 경로를 묶는다.
    pub(super) fn approval_target(&self) -> Result<Value, Error> {
        let conversation = &self.conversation;
        let kind = match conversation.kind {
            ConversationKind::Direct => "direct",
            ConversationKind::Channel => "channel",
            _ => return Err(Error::Invalid),
        };
        if !is_name(&self.binding.0)
            || !identifier(&conversation.id)
            || conversation
                .thread
                .as_deref()
                .is_some_and(|t| !identifier(t))
        {
            return Err(Error::Invalid);
        }
        Ok(json!({"binding": self.binding.0,
            "conversation": {"kind": kind, "id": conversation.id, "thread": conversation.thread}}))
    }
}

impl DeliveryDraft {
    /// outbox 등록 커밋에 저장할 영수증. 답 하나가 대기열 한 건이며 첨부는 그 답에 포함된다.
    /// 재조회는 이때 저장한 바이트를 반환한다.
    pub fn queued_receipt(&self) -> JsonPayload {
        JsonPayload(json!({ "queued": self.answers.len() }).to_string())
    }

    /// 승인자에게 보여 주고 원장이 검증하는 같은 문서. 답·첨부 순서와 줄바꿈을 보존한다.
    /// text/plain의 data만 UTF-8 원문으로 풀어 text로 표시하므로 승인 내용에서 발송 바이트를 복원할 수 있다.
    pub fn approval_json(&self) -> Result<String, Error> {
        if !identifier(&self.activation.0) || self.answers.is_empty() {
            return Err(Error::Invalid);
        }
        let target = self.target.approval_target()?;
        let mut answers = Vec::new();
        let mut remaining = CARD_CAPACITY;
        for answer in &self.answers {
            // JSON 공백·이스케이프·base64를 허용하되 카드에 담지 못할 큰 입력은 파싱 전에 멈춘다.
            if answer.0.len() > 8 * CARD_CAPACITY {
                return Err(Error::Invalid);
            }
            let value = visible_answer(&answer.0)?;
            remaining = remaining
                .checked_sub(value.to_string().len())
                .ok_or(Error::Invalid)?;
            answers.push(value);
        }
        let document = json!({
            "activation": self.activation.0,
            "target": target,
            "answers": answers
        });
        canon::canonicalize(&document)
            .filter(|text| text.len() <= CARD_CAPACITY)
            .ok_or(Error::Invalid)
    }

    /// begin·승인·enqueue 모두 이 지문을 사용한다. 해시 입력은 approval_json()의 UTF-8 바이트다.
    pub fn sha256(&self) -> Result<[u8; 32], Error> {
        Ok(canon::sha256(&self.approval_json()?))
    }
}

fn identifier(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}

fn visible_answer(raw: &str) -> Result<Value, Error> {
    let mut value = canon::parse(raw).ok_or(Error::Invalid)?;
    let body = value.as_object_mut().ok_or(Error::Invalid)?;
    if body
        .keys()
        .any(|k| !matches!(k.as_str(), "text" | "attachments"))
    {
        return Err(Error::Invalid);
    }
    let has_text = match body.get("text") {
        None => false,
        Some(Value::String(s)) if !s.trim().is_empty() => true,
        _ => return Err(Error::Invalid),
    };
    let mut has_files = false;
    if let Some(files) = body.get_mut("attachments") {
        let files = files
            .as_array_mut()
            .filter(|f| f.len() <= 8)
            .ok_or(Error::Invalid)?;
        has_files = !files.is_empty();
        for file in files {
            let map = file.as_object_mut().ok_or(Error::Invalid)?;
            let name = map
                .get("name")
                .and_then(Value::as_str)
                .ok_or(Error::Invalid)?;
            if !identifier(name)
                || name.len() > 255
                || matches!(name, "." | "..")
                || name.contains(['/', '\\'])
                || map.len() != 3
                || map.get("type").and_then(Value::as_str) != Some("text/plain")
            {
                return Err(Error::Invalid);
            }
            let Some(Value::String(data)) = map.remove("data") else {
                return Err(Error::Invalid);
            };
            let bytes = STANDARD.decode(data).map_err(|_| Error::Invalid)?;
            let text = String::from_utf8(bytes).map_err(|_| Error::Invalid)?;
            if text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
            {
                return Err(Error::Invalid);
            }
            map.insert("text".to_owned(), Value::String(text));
        }
    }
    if !has_text && !has_files {
        return Err(Error::Invalid);
    }
    Ok(value)
}

#[cfg(test)]
mod tests;
