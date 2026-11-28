//! Plain-text Markdown cleanup, UTF-16 chunking, and inline-keyboard approval card rendering for Telegram.
use crate::reply::Card;
use bot_core::ledger::ApprovalState;
use serde_json::{Value, json};

/// Conservative UTF-16 code-unit limit below Telegram's 4096-unit `sendMessage` ceiling.
pub const MAX_UTF16_UNITS: usize = 3800;
/// Telegram `callback_data` maximum length in UTF-8 bytes.
pub const MAX_CALLBACK_BYTES: usize = 64;
/// Telegram `sendDocument` / `sendPhoto` caption limit (conservative margin under 1024 UTF-16 units).
pub const MAX_CAPTION_UTF16: usize = 950;

const APPROVE_PREFIX: &str = "bot:a:";
const DENY_PREFIX: &str = "bot:d:";

pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Converts Markdown-heavy model output into clean Telegram plain text without corrupting URLs, code, or identifiers.
pub fn plain_text(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut idx = 0usize;
    let mut fence: Option<(char, usize)> = None;

    while idx < lines.len() {
        let line = lines[idx];
        let trimmed = line.trim_start();
        if let Some((fch, flen)) = fence {
            if is_closing_fence(trimmed, fch, flen) {
                fence = None;
            } else {
                out.push(line.to_owned());
            }
            idx += 1;
            continue;
        }
        if let Some(opened) = opening_fence(trimmed) {
            fence = Some(opened);
            idx += 1;
            continue;
        }
        if idx + 1 < lines.len() && is_table_header(line, lines[idx + 1]) {
            let headers = split_table_cells(line);
            idx += 2;
            while idx < lines.len() && lines[idx].contains('|') && !lines[idx].trim().is_empty() {
                let cells = split_table_cells(lines[idx]);
                if !cells.is_empty() {
                    let row = format_table_row(&headers, &cells);
                    if !row.is_empty() {
                        out.push(row);
                    }
                }
                idx += 1;
            }
            continue;
        }
        if is_horizontal_rule(trimmed) {
            out.push(String::new());
            idx += 1;
            continue;
        }
        out.push(clean_line(line));
        idx += 1;
    }

    let mut collapsed = String::with_capacity(normalized.len());
    let mut blank_run = 0usize;
    for line in out {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !collapsed.is_empty() {
                collapsed.push('\n');
            }
        } else {
            if !collapsed.is_empty() {
                collapsed.push('\n');
            }
            collapsed.push_str(line.trim_end());
            blank_run = 0;
        }
    }
    let result = collapsed.trim().to_owned();
    if result.is_empty() {
        raw.trim().to_owned()
    } else {
        result
    }
}

fn opening_fence(trimmed: &str) -> Option<(char, usize)> {
    let ch = trimmed.chars().next()?;
    if !matches!(ch, '`' | '~') {
        return None;
    }
    let len = trimmed.chars().take_while(|&c| c == ch).count();
    if len < 3 {
        return None;
    }
    let rest = &trimmed[len..];
    if ch == '`' && rest.contains('`') {
        return None;
    }
    Some((ch, len))
}

fn is_closing_fence(trimmed: &str, ch: char, min_len: usize) -> bool {
    let len = trimmed.chars().take_while(|&c| c == ch).count();
    len >= min_len && trimmed[len..].trim().is_empty()
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && (compact.chars().all(|c| c == '-')
            || compact.chars().all(|c| c == '*')
            || compact.chars().all(|c| c == '_'))
}

fn split_table_cells(line: &str) -> Vec<String> {
    let inner = line.trim().trim_matches('|');
    inner
        .split('|')
        .map(|cell| clean_inline(cell.trim()))
        .collect()
}

fn is_table_header(header: &str, sep: &str) -> bool {
    if !header.contains('|') || !sep.contains('|') {
        return false;
    }
    let cells: Vec<&str> = sep.trim().trim_matches('|').split('|').collect();
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let c = cell.trim();
            !c.is_empty()
                && c.chars().all(|ch| matches!(ch, '-' | ':'))
                && c.chars().filter(|&ch| ch == '-').count() >= 3
        })
}

fn format_table_row(headers: &[String], cells: &[String]) -> String {
    let mut parts = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        if cell.is_empty() {
            continue;
        }
        match headers.get(i).filter(|h| !h.is_empty()) {
            Some(header) => parts.push(format!("{header}: {cell}")),
            None => parts.push(cell.clone()),
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("- {}", parts.join(" · "))
    }
}

fn clean_line(line: &str) -> String {
    let mut rest = line;
    let indent_len = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    let indent = &rest[..indent_len];
    rest = &rest[indent_len..];
    while let Some(after) = rest.strip_prefix('>') {
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    // ATX heading: `### Heading` -> `Heading`
    let hashes = rest.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes)
        && rest[hashes..]
            .chars()
            .next()
            .is_some_and(|c| c == ' ' || c == '\t')
    {
        let heading = rest[hashes..].trim().trim_end_matches('#').trim_end();
        return clean_inline(heading);
    }
    // Bullet list marker: `* item` or `+ item` -> `- item`
    if let Some(after) = rest.strip_prefix("* ").or_else(|| rest.strip_prefix("+ ")) {
        return format!("{indent}- {}", clean_inline(after));
    }
    format!("{indent}{}", clean_inline(rest))
}

fn clean_inline(text: &str) -> String {
    let links = strip_markdown_links(text);
    let code = strip_inline_code(&links);
    let strong = strip_delimited(&code, "**", false);
    let strong = strip_delimited(&strong, "__", true);
    let strike = strip_delimited(&strong, "~~", false);
    strip_single_asterisk(&strike)
}

fn strip_markdown_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let is_image = open > 0 && rest.as_bytes()[open - 1] == b'!';
        let prefix_end = if is_image { open - 1 } else { open };
        let after_open = &rest[open + 1..];
        let Some(close_bracket) = after_open.find(']') else {
            out.push_str(rest);
            return out;
        };
        let label = &after_open[..close_bracket];
        let after_label = &after_open[close_bracket + 1..];
        let Some(after_paren) = after_label.strip_prefix('(') else {
            out.push_str(&rest[..open + 1]);
            rest = after_open;
            continue;
        };
        let Some(close_paren) = after_paren.find(')') else {
            out.push_str(rest);
            return out;
        };
        let url = after_paren[..close_paren].trim();
        let valid_url = (url.starts_with("https://") || url.starts_with("http://"))
            && !url.chars().any(char::is_whitespace);
        if valid_url && !label.contains('\n') {
            out.push_str(&rest[..prefix_end]);
            let trimmed_label = label.trim();
            if trimmed_label.is_empty() || trimmed_label == url {
                out.push_str(url);
            } else {
                out.push_str(trimmed_label);
                out.push_str(" (");
                out.push_str(url);
                out.push(')');
            }
            rest = &after_paren[close_paren + 1..];
        } else {
            out.push_str(&rest[..open + 1]);
            rest = after_open;
        }
    }
    out.push_str(rest);
    out
}

fn strip_inline_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('`') else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..open]);
        out.push_str(&after_open[..close]);
        rest = &after_open[close + 1..];
    }
    out.push_str(rest);
    out
}

fn strip_delimited(text: &str, delim: &str, word_boundary: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find(delim) {
        let before = rest[..open].chars().next_back();
        let after_open = &rest[open + delim.len()..];
        let Some(close) = after_open.find(delim) else {
            out.push_str(rest);
            return out;
        };
        let inner = &after_open[..close];
        let after_close = after_open[close + delim.len()..].chars().next();
        let boundary_ok = !word_boundary
            || (!before.is_some_and(|c| c.is_ascii_alphanumeric())
                && !after_close.is_some_and(|c| c.is_ascii_alphanumeric()));
        let inner_ok = !inner.is_empty()
            && !inner.starts_with(char::is_whitespace)
            && !inner.ends_with(char::is_whitespace);
        if boundary_ok && inner_ok {
            out.push_str(&rest[..open]);
            out.push_str(inner);
            rest = &after_open[close + delim.len()..];
        } else {
            out.push_str(&rest[..open + delim.len()]);
            rest = after_open;
        }
    }
    out.push_str(rest);
    out
}

fn strip_single_asterisk(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('*') {
        let before = rest[..open].chars().next_back();
        let after_open = &rest[open + 1..];
        if after_open.starts_with('*') || before.is_some_and(|c| c.is_ascii_alphanumeric()) {
            out.push_str(&rest[..open + 1]);
            rest = after_open;
            continue;
        }
        let Some(close) = after_open.find('*') else {
            out.push_str(rest);
            return out;
        };
        let inner = &after_open[..close];
        let after_close = after_open[close + 1..].chars().next();
        let valid = !inner.is_empty()
            && !inner.starts_with(char::is_whitespace)
            && !inner.ends_with(char::is_whitespace)
            && !after_close.is_some_and(|c| c.is_ascii_alphanumeric() || c == '*');
        if valid {
            out.push_str(&rest[..open]);
            out.push_str(inner);
            rest = &after_open[close + 1..];
        } else {
            out.push_str(&rest[..open + 1]);
            rest = after_open;
        }
    }
    out.push_str(rest);
    out
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

    #[test]
    fn plain_text_strips_markdown_while_preserving_code_urls_and_identifiers() {
        let md = "### 오늘 요약\n\n- **핵심**: `user_id` 및 `foo__bar__baz` 확인\n* *참고*: [돈보라](https://donbora.com/a_b) / 2 * 3 = 6\n\n| 항목 | 값 |\n| --- | --- |\n| Q&A | **1건** |\n\n```sql\nselect user_id from faqs where a * b > 0;\n```\n---\n> 끝";
        let out = plain_text(md);
        assert_eq!(
            out,
            "오늘 요약\n\n- 핵심: user_id 및 foo__bar__baz 확인\n- 참고: 돈보라 (https://donbora.com/a_b) / 2 * 3 = 6\n\n- 항목: Q&A · 값: 1건\n\nselect user_id from faqs where a * b > 0;\n\n끝"
        );
        assert_eq!(
            plain_text("https://donbora.com/x_y"),
            "https://donbora.com/x_y"
        );
        assert_eq!(plain_text("snake_case_name"), "snake_case_name");
        assert_eq!(plain_text("2*3*4"), "2*3*4");
        assert_eq!(plain_text("---"), "---");
    }
}
