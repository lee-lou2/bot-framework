//! Telegram update parsing for group mentions, quoted replies, forum topics, and inbound attachments.
use serde_json::Value;

const MAX_QUOTE_CHARS: usize = 1200;
const MAX_FILE_ID_LEN: usize = 200;

/// Extracts a positive integer ID from a Telegram JSON field.
pub(crate) fn positive_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .filter(|id| *id > 0)
}

/// Extracts a negative signed chat ID (Telegram group / supergroup ID).
pub(crate) fn negative_i64(value: &Value) -> Option<i64> {
    value.as_i64().filter(|id| *id < 0)
}

/// Returns the forum topic ID only when Telegram marks the message as a topic message.
/// Passing `message_thread_id` in non-forum supergroups fails with HTTP 400.
pub(crate) fn forum_topic_id(message: &Value) -> Option<u64> {
    if message["is_topic_message"].as_bool() != Some(true) {
        return None;
    }
    positive_u64(&message["message_thread_id"])
}

/// Slices a UTF-8 string by Telegram's UTF-16 code-unit `offset` and `length`.
pub(crate) fn utf16_slice(text: &str, offset: usize, length: usize) -> Option<(usize, usize)> {
    let target_end = offset.checked_add(length)?;
    let mut unit = 0usize;
    let mut start_byte = (offset == 0).then_some(0usize);
    let mut end_byte = (target_end == 0).then_some(0usize);
    for (byte_idx, ch) in text.char_indices() {
        if unit == offset && start_byte.is_none() {
            start_byte = Some(byte_idx);
        }
        unit += ch.len_utf16();
        let next_byte = byte_idx + ch.len_utf8();
        if unit == target_end && end_byte.is_none() {
            end_byte = Some(next_byte);
        }
        if start_byte.is_some() && end_byte.is_some() {
            break;
        }
    }
    let (start, end) = (start_byte?, end_byte?);
    (start <= end && end <= text.len()).then_some((start, end))
}

fn is_mention_boundary(ch: Option<char>) -> bool {
    ch.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
}

/// Returns whether a group message explicitly mentions `@username` (via entities or boundary-safe scan).
pub(crate) fn mentions_bot(text: &str, entities: Option<&Value>, username: &str) -> bool {
    if username.is_empty() {
        return false;
    }
    let target = format!("@{username}");
    if let Some(list) = entities.and_then(Value::as_array) {
        for entity in list {
            let kind = entity["type"].as_str().unwrap_or_default();
            let (Some(offset), Some(length)) = (
                entity["offset"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok()),
                entity["length"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok()),
            ) else {
                continue;
            };
            let Some((start, end)) = utf16_slice(text, offset, length) else {
                continue;
            };
            let slice = &text[start..end];
            if kind == "mention" && slice.eq_ignore_ascii_case(&target) {
                return true;
            }
            if kind == "bot_command"
                && let Some((_, bot)) = slice.split_once('@')
                && bot.eq_ignore_ascii_case(username)
            {
                return true;
            }
        }
    }
    let lower = text.to_ascii_lowercase();
    let needle = target.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(&needle) {
        let idx = from + rel;
        let end = idx + needle.len();
        let before = text[..idx].chars().next_back();
        let after = text[end..].chars().next();
        if (before != Some('@') && is_mention_boundary(before) || before == Some('/'))
            && is_mention_boundary(after)
        {
            return true;
        }
        from = end;
    }
    false
}

/// Strips `@username` mentions (including `/cmd@username` suffixes) while preserving other text.
pub(crate) fn strip_bot_mention(text: &str, username: &str) -> String {
    if username.is_empty() {
        return text.trim().to_owned();
    }
    let needle = format!("@{}", username.to_ascii_lowercase());
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find(&needle) {
        let idx = cursor + rel;
        let end = idx + needle.len();
        let before = text[..idx].chars().next_back();
        let after = text[end..].chars().next();
        let is_command_suffix = before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && text[..idx]
                .rsplit(|c: char| c.is_whitespace())
                .next()
                .is_some_and(|tok| tok.starts_with('/'));
        let is_plain_mention = is_mention_boundary(before);
        if (is_plain_mention || is_command_suffix) && is_mention_boundary(after) {
            out.push_str(&text[cursor..idx]);
            cursor = end;
        } else {
            out.push_str(&text[cursor..end]);
            cursor = end;
        }
    }
    out.push_str(&text[cursor..]);
    let mut collapsed = String::with_capacity(out.len());
    for (i, line) in out.lines().enumerate() {
        if i > 0 {
            collapsed.push('\n');
        }
        collapsed.push_str(line.trim());
    }
    collapsed.trim().to_owned()
}

/// Returns true when the message directly replies to a non-service message sent by `bot_id`.
pub(crate) fn replies_to_bot(message: &Value, bot_id: u64) -> bool {
    let Some(reply) = message.get("reply_to_message") else {
        return false;
    };
    if is_forum_service_reply(reply) {
        return false;
    }
    positive_u64(&reply["from"]["id"]) == Some(bot_id)
}

fn is_forum_service_reply(reply: &Value) -> bool {
    reply.get("forum_topic_created").is_some()
        && reply["text"].as_str().is_none()
        && reply["caption"].as_str().is_none()
}

fn scrub_snippet(raw: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut prev_ws = false;
    let mut count = 0usize;
    for ch in raw.trim().chars() {
        let mapped = if ch.is_control() || matches!(ch, '\u{85}' | '\u{2028}' | '\u{2029}') {
            ' '
        } else {
            ch
        };
        if mapped.is_whitespace() {
            if prev_ws {
                continue;
            }
            prev_ws = true;
        } else {
            prev_ws = false;
        }
        if count >= max_chars {
            out.push('…');
            break;
        }
        out.push(mapped);
        count += 1;
    }
    out.trim().to_owned()
}

/// Formats untrusted quoted context when the user replied to an earlier message (ignoring forum creation stubs).
pub(crate) fn reply_context_prefix(message: &Value, bot_id: u64) -> Option<String> {
    let reply = message.get("reply_to_message")?;
    if is_forum_service_reply(reply) {
        return None;
    }
    let speaker = match positive_u64(&reply["from"]["id"]) {
        Some(id) if id == bot_id => "bot".to_owned(),
        Some(id) => format!("telegram:{id}"),
        None => "message".to_owned(),
    };
    let quoted = message
        .get("quote")
        .and_then(|q| q["text"].as_str())
        .or_else(|| reply["text"].as_str())
        .or_else(|| reply["caption"].as_str())
        .map(|s| scrub_snippet(s, MAX_QUOTE_CHARS))
        .filter(|s| !s.is_empty());
    let content = match quoted {
        Some(text) => text,
        None if has_media(reply) => "[attachment]".to_owned(),
        None => return None,
    };
    Some(format!("[Replying to {speaker}: \"{content}\"]"))
}

fn valid_file_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_FILE_ID_LEN
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn has_media(message: &Value) -> bool {
    [
        "document",
        "photo",
        "audio",
        "voice",
        "video",
        "animation",
        "video_note",
        "sticker",
    ]
    .iter()
    .any(|field| message.get(*field).is_some_and(|v| !v.is_null()))
}

pub(crate) struct InboundAttachments {
    pub refs: Vec<String>,
    pub summary: Option<String>,
}

/// Extracts host-scoped attachment refs (`telegram:<alias>/<file_id>`) and a safe placeholder summary for captionless uploads.
pub(crate) fn extract_attachments(
    message: &Value,
    alias: &str,
    limit: usize,
) -> InboundAttachments {
    let mut refs = Vec::new();
    let mut labels = Vec::new();
    let mut push = |file_id: Option<&str>, label: String| {
        if refs.len() >= limit {
            return;
        }
        if let Some(id) = file_id.filter(|s| valid_file_id(s)) {
            refs.push(format!("telegram:{alias}/{id}"));
            labels.push(label);
        }
    };
    if let Some(doc) = message.get("document") {
        let name = doc["file_name"]
            .as_str()
            .map(|s| scrub_snippet(s, 80))
            .filter(|s| !s.is_empty());
        let label = match name {
            Some(name) => format!("document {name}"),
            None => "document".to_owned(),
        };
        push(doc["file_id"].as_str(), label);
    }
    if let Some(photos) = message.get("photo").and_then(Value::as_array) {
        let best = photos
            .iter()
            .max_by_key(|p| {
                (
                    p["file_size"].as_u64().unwrap_or(0),
                    p["width"].as_u64().unwrap_or(0) * p["height"].as_u64().unwrap_or(0),
                )
            })
            .or_else(|| photos.last());
        if let Some(photo) = best {
            push(photo["file_id"].as_str(), "photo".to_owned());
        }
    }
    for (field, label) in [
        ("audio", "audio"),
        ("voice", "voice"),
        ("video", "video"),
        ("animation", "animation"),
        ("video_note", "video_note"),
        ("sticker", "sticker"),
    ] {
        if let Some(media) = message.get(field) {
            push(media["file_id"].as_str(), label.to_owned());
        }
    }
    let summary = (!labels.is_empty()).then(|| format!("[Attachment: {}]", labels.join(", ")));
    InboundAttachments { refs, summary }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn utf16_entities_detect_mentions_and_commands_even_after_emojis() {
        // "😀 " is 3 UTF-16 units (2 for surrogate pair + 1 space); "@donbora_bot" is 12 units.
        let text = "😀 @Donbora_Bot check status";
        let entities = json!([{"type": "mention", "offset": 3, "length": 12}]);
        assert!(mentions_bot(text, Some(&entities), "donbora_bot"));
        assert!(!mentions_bot(text, Some(&entities), "other_bot"));

        let cmd = "/status@donbora_bot now";
        let cmd_entities = json!([{"type": "bot_command", "offset": 0, "length": 19}]);
        assert!(mentions_bot(cmd, Some(&cmd_entities), "donbora_bot"));
        assert!(!mentions_bot("@donbora_bot_extra hi", None, "donbora_bot"));
    }

    #[test]
    fn strips_bot_mentions_and_command_targets_cleanly() {
        assert_eq!(
            strip_bot_mention("@donbora_bot 안녕하세요", "donbora_bot"),
            "안녕하세요"
        );
        assert_eq!(
            strip_bot_mention("@DONBORA_BOT  질문", "donbora_bot"),
            "질문"
        );
        assert_eq!(
            strip_bot_mention("/summary@donbora_bot 오늘 요약", "donbora_bot"),
            "/summary 오늘 요약"
        );
        assert_eq!(
            strip_bot_mention("@donbora_bot_2 keep me", "donbora_bot"),
            "@donbora_bot_2 keep me"
        );
        assert_eq!(strip_bot_mention("@donbora_bot", "donbora_bot"), "");
    }

    #[test]
    fn extracts_quoted_reply_context_and_ignores_forum_topic_stubs() {
        let normal_reply = json!({
            "reply_to_message": {
                "from": {"id": 999},
                "text": "Q&A #210 draft\nline 2"
            }
        });
        assert_eq!(
            reply_context_prefix(&normal_reply, 999).as_deref(),
            Some("[Replying to bot: \"Q&A #210 draft line 2\"]")
        );
        let partial_quote = json!({
            "quote": {"text": "only this part"},
            "reply_to_message": {"from": {"id": 555}, "text": "whole long message"}
        });
        assert_eq!(
            reply_context_prefix(&partial_quote, 999).as_deref(),
            Some("[Replying to telegram:555: \"only this part\"]")
        );
        let forum_stub = json!({
            "is_topic_message": true,
            "message_thread_id": 77,
            "reply_to_message": {"from": {"id": 999}, "forum_topic_created": {"name": "General"}}
        });
        assert!(!replies_to_bot(&forum_stub, 999));
        assert_eq!(reply_context_prefix(&forum_stub, 999), None);
        assert_eq!(forum_topic_id(&forum_stub), Some(77));
        assert_eq!(forum_topic_id(&json!({"message_thread_id": 77})), None);
    }

    #[test]
    fn extracts_largest_photo_and_document_refs() {
        let msg = json!({
            "document": {"file_id": "BQACAgIAAxkBA123", "file_name": "report.pdf"},
            "photo": [
                {"file_id": "small_1", "width": 90, "height": 90, "file_size": 1000},
                {"file_id": "large_2", "width": 1280, "height": 720, "file_size": 85000}
            ]
        });
        let got = extract_attachments(&msg, "ops", 8);
        assert_eq!(
            got.refs,
            ["telegram:ops/BQACAgIAAxkBA123", "telegram:ops/large_2"]
        );
        assert_eq!(
            got.summary.as_deref(),
            Some("[Attachment: document report.pdf, photo]")
        );
    }
}
