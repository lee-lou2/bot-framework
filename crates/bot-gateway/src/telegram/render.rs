//! Plain-text UTF-16 chunking and inline-keyboard approval card rendering for Telegram.
use crate::reply::Card;
use bot_core::ledger::ApprovalState;
use serde_json::{Value, json};

/// Conservative UTF-16 code-unit limit below Telegram's 4096-unit `sendMessage` ceiling.
pub const MAX_UTF16_UNITS: usize = 3800;
/// Telegram `callback_data` maximum length in UTF-8 bytes.
pub const MAX_CALLBACK_BYTES: usize = 64;
/// Telegram `sendDocument` caption limit (conservative margin under 1024 UTF-16 units).
pub const MAX_CAPTION_UTF16: usize = 950;

const APPROVE_PREFIX: &str = "bot:a:";
const DENY_PREFIX: &str = "bot:d:";

pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Splits plain text into chunks each at most `limit` UTF-16 code units without breaking UTF-8 scalars.
pub fn split_plain(text: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return vec![];
    }
    if utf16_len(text) <= limit {
        return vec![text.to_owned()];
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if utf16_len(rest) <= limit {
            chunks.push(rest.to_owned());
            break;
        }
        let mut units = 0usize;
        let mut end = 0usize;
        let mut last_para = None;
        let mut last_nl = None;
        let mut last_ws = None;
        for (idx, ch) in rest.char_indices() {
            let w = ch.len_utf16();
            if units + w > limit {
                break;
            }
            units += w;
            let next = idx + ch.len_utf8();
            end = next;
            if ch == '\n' {
                last_nl = Some(next);
                if rest[..idx].ends_with('\n') {
                    last_para = Some(next);
                }
            } else if ch.is_whitespace() {
                last_ws = Some(next);
            }
        }
        let cut = last_para
            .or(last_nl)
            .or(last_ws)
            .filter(|pos| *pos > 0)
            .unwrap_or(end.max(1));
        let (head, tail) = rest.split_at(cut);
        let head_trimmed = head.trim_end_matches([' ', '\t', '\n']);
        if !head_trimmed.is_empty() {
            chunks.push(head_trimmed.to_owned());
        } else if !head.is_empty() {
            chunks.push(head.to_owned());
        }
        rest = tail.trim_start_matches('\n');
    }
    chunks
}

pub fn callback_data(allow: bool, nonce: &str) -> Option<String> {
    let prefix = if allow { APPROVE_PREFIX } else { DENY_PREFIX };
    let data = format!("{prefix}{nonce}");
    (data.len() <= MAX_CALLBACK_BYTES && !nonce.is_empty()).then_some(data)
}

pub fn parse_callback(data: &str) -> Option<(bool, &str)> {
    let (allow, nonce) = if let Some(nonce) = data.strip_prefix(APPROVE_PREFIX) {
        (true, nonce)
    } else {
        let nonce = data.strip_prefix(DENY_PREFIX)?;
        (false, nonce)
    };
    let valid =
        !nonce.is_empty() && nonce.len() <= 200 && nonce.bytes().all(|b| b.is_ascii_graphic());
    valid.then_some((allow, nonce))
}

pub fn card_text(card: &Card) -> String {
    let mut out = format!("Approval required: {}", card.title);
    for line in &card.lines {
        out.push('\n');
        out.push_str(line);
    }
    out
}

pub fn resolved_card_text(card: &Card, status: &str) -> String {
    format!("{}\n\n{status}", card_text(card))
}

pub fn card_fits(card: &Card) -> bool {
    callback_data(true, &card.nonce).is_some()
        && callback_data(false, &card.nonce).is_some()
        && utf16_len(&card_text(card)) + 64 <= MAX_UTF16_UNITS
}

pub fn card_keyboard(card: &Card) -> Option<Value> {
    let approve = callback_data(true, &card.nonce)?;
    let deny = callback_data(false, &card.nonce)?;
    Some(json!({
        "inline_keyboard": [[
            {"text": "Approve", "callback_data": approve},
            {"text": "Deny", "callback_data": deny}
        ]]
    }))
}

pub fn status_line(state: ApprovalState) -> Option<&'static str> {
    match state {
        ApprovalState::Pending => None,
        ApprovalState::Approved | ApprovalState::Consumed => Some("✅ Approved"),
        ApprovalState::Denied => Some("⛔ Denied"),
        ApprovalState::Expired => Some("⌛ Expired"),
        ApprovalState::Superseded => Some("↻ Conditions changed; new approval required"),
        ApprovalState::Revoked => Some("⛔ Revoked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_long_text_by_utf16_units_including_surrogate_pair_emojis() {
        // Each emoji counts as 2 UTF-16 code units.
        let emojis = "😀".repeat(2500);
        let parts = split_plain(&emojis, MAX_UTF16_UNITS);
        assert_eq!(parts.len(), 2);
        for part in &parts {
            assert!(utf16_len(part) <= MAX_UTF16_UNITS);
        }
        assert_eq!(parts.concat(), emojis);

        let lines = format!("{}\n{}", "a".repeat(3000), "b".repeat(1500));
        let split = split_plain(&lines, MAX_UTF16_UNITS);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0], "a".repeat(3000));
        assert_eq!(split[1], "b".repeat(1500));
    }

    #[test]
    fn callback_round_trip_and_length_bounds() {
        let nonce = "a".repeat(32);
        let a = callback_data(true, &nonce).unwrap();
        let d = callback_data(false, &nonce).unwrap();
        assert_eq!(parse_callback(&a), Some((true, nonce.as_str())));
        assert_eq!(parse_callback(&d), Some((false, nonce.as_str())));
        assert_eq!(parse_callback("other:x"), None);
        assert_eq!(parse_callback("bot:a:"), None);
        assert!(callback_data(true, &"x".repeat(59)).is_none());
    }
}
