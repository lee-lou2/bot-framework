//! Pure Slack Block Kit builders for markdown replies, trailing note context blocks, and non-truncated `plain_text` approval cards.
//!
//! Card lines use `plain_text` so injected mentions or links cannot render; oversized cards are rejected rather than truncated.
use crate::reply::Card;
use serde_json::{Value, json};

pub const APPROVE: &str = "bot_approve";
pub const DENY: &str = "bot_deny";
pub const TITLE_LIMIT: usize = 150;
pub const SECTION_LIMIT: usize = 3_000;
/// Below Slack's 12,000-char markdown block limit; larger bodies fall back to plain text without truncation.
pub const MARKDOWN_LIMIT: usize = 11_000;
/// Below Slack's 40,000-char message truncation point.
pub const PLAIN_LIMIT: usize = 39_000;

/// Escapes broadcast mentions (`<!channel>`, `<!here>`, `<!subteam^...>`) to literal text.
pub fn neutralize(text: &str) -> String {
    text.replace("<!", "&lt;!")
}

pub fn markdown_block(text: &str) -> Value {
    json!({"type": "markdown", "text": neutralize(text)})
}

pub fn fits_block(text: &str) -> bool {
    neutralize(text).chars().count() <= MARKDOWN_LIMIT
}

fn context_block(mrkdwn: &str) -> Value {
    json!({"type": "context", "elements": [{"type": "mrkdwn", "text": mrkdwn}]})
}

/// Builds Slack Block Kit blocks for a markdown reply, returning `None` if it exceeds block limits.
/// Trailing `※` note lines split into a smaller `context` block when both body and note fit intact;
/// oversized notes stay inside the main markdown block rather than being truncated.
pub fn text_blocks(text: &str) -> Option<Vec<Value>> {
    if let Some((body, note)) = split_trailing_notes(text) {
        let rendered = neutralize(&md_to_mrkdwn(&note));
        if fits_block(&body) && rendered.chars().count() <= SECTION_LIMIT {
            return Some(vec![markdown_block(&body), context_block(&rendered)]);
        }
    }
    fits_block(text).then(|| vec![markdown_block(text)])
}

/// Splits trailing `※` footnote lines (skipping blank lines between them) from a reply body.
/// Returns `None` when no trailing notes exist, the remaining body would be empty, or the split
/// would fall inside an unclosed fenced code block.
pub fn split_trailing_notes(text: &str) -> Option<(String, String)> {
    let mut lines: Vec<&str> = text.trim_end().lines().collect();
    let mut notes = Vec::new();
    while let Some(&last) = lines.last() {
        let trimmed = last.trim();
        if trimmed.is_empty() {
            lines.pop();
        } else if trimmed.starts_with('※') {
            notes.push(trimmed);
            lines.pop();
        } else {
            break;
        }
    }
    if notes.is_empty() {
        return None;
    }
    let body = lines.join("\n");
    let body = body.trim_end();
    if body.trim().is_empty() || unclosed_code_fence(body) {
        return None;
    }
    notes.reverse();
    Some((body.to_owned(), notes.join("\n")))
}

fn unclosed_code_fence(text: &str) -> bool {
    let mut fence: Option<(char, usize)> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some((ch, min_len)) = fence {
            let len = trimmed.chars().take_while(|&c| c == ch).count();
            if len >= min_len && trimmed[len..].trim().is_empty() {
                fence = None;
            }
        } else if let Some(ch) = trimmed.chars().next().filter(|c| matches!(c, '`' | '~')) {
            let len = trimmed.chars().take_while(|&c| c == ch).count();
            if len >= 3 && (ch != '`' || !trimmed[len..].contains('`')) {
                fence = Some((ch, len));
            }
        }
    }
    fence.is_some()
}

/// Converts standard Markdown syntax used in footnote lines into Slack `mrkdwn` for `context` blocks.
pub fn md_to_mrkdwn(md: &str) -> String {
    let mut out = Vec::new();
    let mut in_code = false;
    for line in md.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.push(line.to_owned());
            continue;
        }
        if in_code {
            out.push(line.to_owned());
            continue;
        }
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            out.push(format!("*{}*", inline_mrkdwn(trimmed[hashes..].trim())));
            continue;
        }
        out.push(inline_mrkdwn(line));
    }
    out.join("\n")
}

fn inline_mrkdwn(s: &str) -> String {
    let bold = pair_swap_outside_code(s, "**", "*");
    let strike = pair_swap_outside_code(&bold, "~~", "~");
    links_outside_code(&strike)
}

fn pair_swap_outside_code(s: &str, from: &str, to: &str) -> String {
    let mut positions = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with('`')
            && let Some(close) = rest[1..].find('`')
        {
            i += 1 + close + 1;
            continue;
        }
        if rest.starts_with(from) {
            positions.push(i);
            i += from.len();
            continue;
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    if positions.len() < 2 || !positions.len().is_multiple_of(2) {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut prev = 0;
    for pos in positions {
        out.push_str(&s[prev..pos]);
        out.push_str(to);
        prev = pos + from.len();
    }
    out.push_str(&s[prev..]);
    out
}

fn links_outside_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('`') {
        out.push_str(&links(&rest[..open]));
        let after = &rest[open + 1..];
        match after.find('`') {
            Some(close) => {
                out.push('`');
                out.push_str(&after[..close]);
                out.push('`');
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&links(&rest[open..]));
                return out;
            }
        }
    }
    out.push_str(&links(rest));
    out
}

fn links(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let Some(mid) = rest[open..].find("](") else {
            break;
        };
        let Some(close) = rest[open + mid..].find(')') else {
            break;
        };
        let label = &rest[open + 1..open + mid];
        let url = &rest[open + mid + 2..open + mid + close];
        let valid_url = (url.starts_with("http://") || url.starts_with("https://"))
            && !url.contains(['<', '>', '|'])
            && !url.chars().any(char::is_whitespace);
        let valid_label = !label.is_empty() && !label.contains(['\n', '[', ']', '<', '>', '|']);
        if !valid_url || !valid_label {
            out.push_str(&rest[..open + 1]);
            rest = &rest[open + 1..];
            continue;
        }
        out.push_str(&rest[..open]);
        out.push_str(&format!("<{url}|{label}>"));
        rest = &rest[open + mid + close + 1..];
    }
    out.push_str(rest);
    out
}

pub fn card_fits(card: &Card) -> bool {
    card.title.chars().count() <= TITLE_LIMIT && card_body(card).chars().count() <= SECTION_LIMIT
}

fn card_body(card: &Card) -> String {
    card.lines.join("\n")
}

fn card_parts(card: &Card) -> Vec<Value> {
    let mut blocks = vec![
        json!({"type": "header", "text": {"type": "plain_text", "text": card.title, "emoji": false}}),
    ];
    let body = card_body(card);
    if !body.is_empty() {
        blocks.push(json!({"type": "section", "text": {"type": "plain_text", "text": body, "emoji": false}}));
    }
    blocks
}

pub fn card_blocks(card: &Card) -> Vec<Value> {
    let mut blocks = card_parts(card);
    blocks.push(json!({"type": "actions", "elements": [
        {"type": "button", "text": {"type": "plain_text", "text": "Approve"}, "style": "primary",
         "action_id": APPROVE, "value": card.nonce},
        {"type": "button", "text": {"type": "plain_text", "text": "Deny"}, "style": "danger",
         "action_id": DENY, "value": card.nonce},
    ]}));
    blocks
}

pub fn resolved_blocks(card: &Card, status: &str) -> Vec<Value> {
    let mut blocks = card_parts(card);
    blocks.push(json!({"type": "context", "elements": [{"type": "plain_text", "text": status, "emoji": false}]}));
    blocks
}

pub fn card_text(card: &Card) -> String {
    let body = card_body(card);
    if body.is_empty() {
        card.title.clone()
    } else {
        format!("{}\n{body}", card.title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> Card {
        Card {
            nonce: "n-1".into(),
            title: "Delete event".into(),
            lines: vec![
                "Target: Walk <!channel>".into(),
                "This action cannot be undone".into(),
            ],
        }
    }

    #[test]
    fn broadcast_mentions_are_neutralised_but_user_markup_is_left_alone() {
        assert_eq!(
            neutralize("hi <!channel> and <!here> <!subteam^S1> <@U1>"),
            "hi &lt;!channel> and &lt;!here> &lt;!subteam^S1> <@U1>"
        );
        let block = markdown_block("<!everyone> hello");
        assert_eq!(block["text"], "&lt;!everyone> hello");
        assert_eq!(
            markdown_block(&"x".repeat(MARKDOWN_LIMIT))["text"]
                .as_str()
                .unwrap()
                .len(),
            MARKDOWN_LIMIT
        );
        assert!(fits_block(&"x".repeat(MARKDOWN_LIMIT)));
        assert!(!fits_block(&"x".repeat(MARKDOWN_LIMIT + 1)));
        assert!(!fits_block(&"<!".repeat(MARKDOWN_LIMIT / 2)));
    }

    #[test]
    fn cards_use_plain_text_and_carry_the_nonce_on_both_buttons() {
        let blocks = card_blocks(&card());
        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks[1]["text"]["type"], "plain_text");
        assert!(
            blocks[1]["text"]["text"]
                .as_str()
                .unwrap()
                .contains("<!channel>")
        );
        let buttons = blocks[2]["elements"].as_array().unwrap();
        let ids: Vec<_> = buttons
            .iter()
            .map(|b| {
                (
                    b["action_id"].as_str().unwrap(),
                    b["value"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(ids, [(APPROVE, "n-1"), (DENY, "n-1")]);
        let resolved = resolved_blocks(&card(), "Approved");
        assert!(resolved.iter().all(|b| b["type"] != "actions"));
        assert_eq!(resolved.last().unwrap()["elements"][0]["text"], "Approved");
    }

    #[test]
    fn a_card_that_would_be_cut_is_not_sent_with_buttons() {
        assert!(card_fits(&card()));
        let mut long_title = card();
        long_title.title = "★".repeat(TITLE_LIMIT + 1);
        assert!(!card_fits(&long_title));
        let mut long_body = card();
        long_body.lines = vec!["★".repeat(500); 7];
        assert!(!card_fits(&long_body));
        assert_eq!(
            card_text(&card()),
            "Delete event\nTarget: Walk <!channel>\nThis action cannot be undone"
        );
    }

    #[test]
    fn trailing_note_lines_split_off_without_affecting_middle_or_note_only_text() {
        assert_eq!(
            split_trailing_notes("**Summary**\n- Item\n\n※ Source: A\n\n※ Caveat: today\n"),
            Some((
                "**Summary**\n- Item".into(),
                "※ Source: A\n※ Caveat: today".into()
            ))
        );
        assert_eq!(
            split_trailing_notes("```txt\nline\n```\n\n※ After code"),
            Some(("```txt\nline\n```".into(), "※ After code".into()))
        );
        for untouched in [
            "plain reply",
            "※ note only",
            "※ first\n※ second",
            "before\n※ middle\nafter",
            "```txt\nunclosed\n※ inside fence",
        ] {
            assert_eq!(split_trailing_notes(untouched), None, "{untouched}");
        }
    }

    #[test]
    fn markdown_in_notes_converts_to_slack_mrkdwn_safely() {
        assert_eq!(
            md_to_mrkdwn("# Heading\n**bold** and ~~strike~~ [docs](https://example.com)"),
            "*Heading*\n*bold* and ~strike~ <https://example.com|docs>"
        );
        assert_eq!(
            md_to_mrkdwn("`a**b**c [x](https://a.com)` and **bold `code` text**"),
            "`a**b**c [x](https://a.com)` and *bold `code` text*"
        );
        assert_eq!(md_to_mrkdwn("```\n**keep**\n```"), "```\n**keep**\n```");
        assert_eq!(md_to_mrkdwn("unmatched ** stars"), "unmatched ** stars");
        assert_eq!(
            md_to_mrkdwn("[tag] [link](https://example.com/p)"),
            "[tag] <https://example.com/p|link>"
        );
        assert_eq!(
            md_to_mrkdwn("[bad](javascript:alert(1)) [x](https://a.com/a|b) [a>b](https://a.com)"),
            "[bad](javascript:alert(1)) [x](https://a.com/a|b) [a>b](https://a.com)"
        );
    }

    #[test]
    fn text_blocks_place_trailing_notes_in_a_context_block_without_truncating() {
        let blocks = text_blocks(
            "Done.\n\n※ Verified with **tool** (`ok`), [ref](https://example.com), <!here> <@U1>",
        )
        .unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "markdown");
        assert_eq!(blocks[0]["text"], "Done.");
        assert_eq!(blocks[1]["type"], "context");
        assert_eq!(blocks[1]["elements"][0]["type"], "mrkdwn");
        assert_eq!(
            blocks[1]["elements"][0]["text"],
            "※ Verified with *tool* (`ok`), <https://example.com|ref>, &lt;!here> <@U1>"
        );

        // Oversized notes stay intact inside the main markdown block instead of truncating.
        let long_note = format!("Done.\n\n※ {}", "x".repeat(SECTION_LIMIT));
        let fallback = text_blocks(&long_note).unwrap();
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0]["type"], "markdown");
        assert_eq!(fallback[0]["text"], long_note);

        // Body and note are each checked against their respective Slack block limits.
        let split_fit = format!("{}\n\n※ {}", "a".repeat(10_500), "b".repeat(1_000));
        let split_blocks = text_blocks(&split_fit).unwrap();
        assert_eq!(split_blocks.len(), 2);
        assert_eq!(split_blocks[0]["type"], "markdown");
        assert_eq!(split_blocks[1]["type"], "context");

        let oversized_body = format!("{}\n\n※ note", "a".repeat(MARKDOWN_LIMIT + 1));
        assert!(text_blocks(&oversized_body).is_none());
    }
}
