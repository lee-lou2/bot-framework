//! Pure Slack Block Kit builders for markdown replies and non-truncated `plain_text` approval cards.
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
}
