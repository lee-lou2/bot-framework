//! 회신 본문 검증. 본문은 text(최대 12,000자)·attachments(최대 8개, 디코딩 합계 8 MiB, 각 name/type/data) 중
//! 하나 이상인 JSON 객체다. approval 카드는 브로커가 승인 원장과 같은 커밋에 만든 것만 허용하므로 일반 답에서는 거절한다.

use crate::{codec::*, db::R};
use bot_core::command::JsonPayload;
use serde_json::{Map, Value};

const MAX_TEXT_CHARS: usize = 12_000;
const MAX_ATTACHMENTS: usize = 8;
const MAX_ATTACHMENT_BYTES: usize = 8 << 20;
const MAX_CARD_LINES: usize = 20;

/// base64 문자열이 디코딩되면 몇 바이트인가(형식이 잘못이면 None).
fn decoded_len(data: &str) -> Option<usize> {
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '-' | '_');
    let body = data.trim_end_matches('=');
    if data.len() - body.len() > 2 || !body.chars().all(ok) {
        return None;
    }
    Some(body.len() * 3 / 4)
}

fn text_ok(map: &Map<String, Value>) -> R<bool> {
    match map.get("text") {
        None => Ok(false),
        Some(Value::String(text)) if text.chars().count() <= MAX_TEXT_CHARS => Ok(true),
        Some(_) => invalid(),
    }
}

fn attachments_ok(map: &Map<String, Value>) -> R<bool> {
    let Some(list) = map.get("attachments") else {
        return Ok(false);
    };
    let Some(list) = list
        .as_array()
        .filter(|l| !l.is_empty() && l.len() <= MAX_ATTACHMENTS)
    else {
        return invalid();
    };
    let mut total = 0usize;
    for item in list {
        let field = |name: &str| item.get(name).and_then(Value::as_str);
        let (Some(name), Some(kind), Some(data)) = (field("name"), field("type"), field("data"))
        else {
            return invalid();
        };
        if name.is_empty()
            || name.len() > 255
            || kind.is_empty()
            || kind.len() > 128
            || name.chars().any(char::is_control)
        {
            return invalid();
        }
        total += decoded_len(data).ok_or(bot_core::port::Error::Invalid)?;
    }
    if total > MAX_ATTACHMENT_BYTES {
        invalid()
    } else {
        Ok(true)
    }
}

/// 일반 답(`Finish.answers`)을 검증하고 정규화한 문서 글을 돌려준다.
pub(crate) fn answer(payload: &JsonPayload) -> R<String> {
    let (value, canonical_text) = canonical(payload)?;
    let Some(map) = value.as_object() else {
        return invalid();
    };
    if map.contains_key("approval")
        || map
            .keys()
            .any(|k| !matches!(k.as_str(), "text" | "attachments"))
    {
        return invalid();
    }
    if text_ok(map)? | attachments_ok(map)? {
        Ok(canonical_text)
    } else {
        invalid()
    }
}

/// 승인 카드(브로커가 `open_approval`에 싣는 문서)를 검증한다. `{"approval": {"nonce", "title", "lines"}}`이고
/// nonce는 이 승인의 것이어야 한다. 선택적으로 text를 함께 둘 수 있다.
pub(crate) fn card(payload: &JsonPayload, nonce: &str) -> R<String> {
    let (value, canonical_text) = canonical(payload)?;
    let Some(map) = value.as_object() else {
        return invalid();
    };
    if map
        .keys()
        .any(|k| !matches!(k.as_str(), "approval" | "text"))
    {
        return invalid();
    }
    text_ok(map)?;
    let Some(card) = map.get("approval").and_then(Value::as_object) else {
        return invalid();
    };
    let lines_ok = card
        .get("lines")
        .and_then(Value::as_array)
        .is_some_and(|lines| {
            lines.len() <= MAX_CARD_LINES
                && lines
                    .iter()
                    .all(|l| l.as_str().is_some_and(|s| s.len() <= 500))
        });
    let title_ok = card
        .get("title")
        .and_then(Value::as_str)
        .is_some_and(|t| !t.is_empty() && t.len() <= 200);
    if card.get("nonce").and_then(Value::as_str) != Some(nonce) || !lines_ok || !title_ok {
        return invalid();
    }
    Ok(canonical_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::attachments::{pack_answers, split};

    #[test]
    fn packed_inline_attachments_pass_store_answer_validation() {
        let raw = "결론이에요.\n\n<첨부 이름=\"근거.md\">\n## 상세\n- 1건\n</첨부>";
        let (body, files) = split(raw);
        let (packed, truncated) = pack_answers(&files, 15);
        assert!(!truncated);
        assert!(
            answer(&JsonPayload(
                serde_json::json!({ "text": body }).to_string()
            ))
            .is_ok()
        );
        for payload in &packed {
            assert!(answer(payload).is_ok());
        }
    }
}
