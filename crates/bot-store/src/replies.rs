//! Outbox reply payload validation (`text`, `attachments`, and broker-created `approval` cards).

use crate::{codec::*, db::R};
use bot_core::command::JsonPayload;
use serde_json::{Map, Value};

const MAX_TEXT_CHARS: usize = 12_000;
const MAX_ATTACHMENTS: usize = 8;
const MAX_ATTACHMENT_BYTES: usize = 8 << 20;
const MAX_CARD_LINES: usize = 20;

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

/// Validates and canonicalizes a normal reply (`text` and/or `attachments`; rejects `approval`).
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

/// Validates a broker-emitted approval card payload (`approval.{nonce,title,lines}` plus optional `text`).
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
        let raw =
            "Summary.\n\n<attachment name=\"evidence.md\">\n## Details\n- 1 item\n</attachment>";
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
