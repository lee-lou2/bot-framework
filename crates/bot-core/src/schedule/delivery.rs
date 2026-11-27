//! Scheduled explicit-destination delivery (`schedule.deliver`): constants, approval draft rendering, and route sealing.

use super::{DeliveryDraft, DeliveryTarget};
use crate::{canon, command::JsonPayload, id::is_name, port::Error, state::ConversationKind};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

/// Host-owned call ID; at most one final delivery bundle per scheduled run.
pub const DELIVER_CALL: &str = "schedule-deliver";
pub use crate::builtin::{DELIVER_TOOL, REVISION as DELIVER_REVISION};
pub const DELIVERY_CARD_LABEL: &str = "Delivery draft";

// Gateway cards allow 20 lines x 500 bytes; the renderer also counts header lines.
const CARD_CAPACITY: usize = 20 * 500;

impl DeliveryTarget {
    /// Canonical target representation shared by schedule creation and delivery.
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
    /// Receipt committed on outbox enqueue (one row per answer, attachments included); replayed verbatim on retry.
    pub fn queued_receipt(&self) -> JsonPayload {
        JsonPayload(json!({ "queued": self.answers.len() }).to_string())
    }

    /// Canonical JSON shown to the approver and verified by the ledger, preserving answer/attachment order and newlines.
    /// Decodes `text/plain` attachment `data` into UTF-8 `text` so sent bytes are reconstructible from what was approved.
    pub fn approval_json(&self) -> Result<String, Error> {
        if !identifier(&self.activation.0) || self.answers.is_empty() {
            return Err(Error::Invalid);
        }
        let target = self.target.approval_target()?;
        let mut answers = Vec::new();
        let mut remaining = CARD_CAPACITY;
        for answer in &self.answers {
            // Reject oversized raw payloads before parsing while allowing JSON whitespace/escapes/base64.
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

    /// Digest shared by `begin`, approval, and `enqueue`, hashed over `approval_json()` UTF-8 bytes.
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
