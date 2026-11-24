//! Slack 블록 조립(순수). 승인 카드는 브로커가 렌더링한 제목·줄을 그대로 보여 주고(잘라서 보이지 않는다),
//! 줄은 서식을 해석하지 않는 `plain_text`로 둬 멘션·링크가 끼어들 수 없다.
use crate::reply::Card;
use serde_json::{Value, json};

pub const APPROVE: &str = "bot_approve";
pub const DENY: &str = "bot_deny";
/// Slack 헤더 블록(150자)·섹션 텍스트(3,000자) 한도
pub const TITLE_LIMIT: usize = 150;
pub const SECTION_LIMIT: usize = 3_000;
/// markdown 블록 하나에 본문을 담는 한도(Slack 한도 12,000자보다 아래). 넘는 본문은 자르지 않고 글자판 메시지로 보낸다.
pub const MARKDOWN_LIMIT: usize = 11_000;
/// 글자판(`text`) 메시지 한 개의 본문 한도(Slack은 40,000자에서 자른다). 넘으면 보내지 않는다.
pub const PLAIN_LIMIT: usize = 39_000;

/// 전체 알림 멘션(`<!channel>` `<!here>` `<!subteam^…>` 등)을 글자 그대로 보이게 만든다.
pub fn neutralize(text: &str) -> String {
    text.replace("<!", "&lt;!")
}

/// 본문: markdown 블록 하나(표·목록·코드 렌더링). **자르지 않는다** — 한도(`MARKDOWN_LIMIT`)에 들어가는지는 부르는 쪽이 `fits_block`으로 먼저 본다.
pub fn markdown_block(text: &str) -> Value {
    json!({"type": "markdown", "text": neutralize(text)})
}

/// 알림 멘션을 글자 그대로 바꾼 본문이 markdown 블록 하나에 다 들어가는가
pub fn fits_block(text: &str) -> bool {
    neutralize(text).chars().count() <= MARKDOWN_LIMIT
}

/// 카드를 있는 그대로 담을 수 있는가. 못 담으면 보내지 않고 상세 승인 화면으로 연결한다.
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

/// 승인·거절 버튼이 달린 카드. 버튼 값은 승인 nonce다.
pub fn card_blocks(card: &Card) -> Vec<Value> {
    let mut blocks = card_parts(card);
    blocks.push(json!({"type": "actions", "elements": [
        {"type": "button", "text": {"type": "plain_text", "text": "승인"}, "style": "primary",
         "action_id": APPROVE, "value": card.nonce},
        {"type": "button", "text": {"type": "plain_text", "text": "거절"}, "style": "danger",
         "action_id": DENY, "value": card.nonce},
    ]}));
    blocks
}

/// 처리된 카드: 버튼을 떼고 원장 상태를 한 줄로 보인다.
pub fn resolved_blocks(card: &Card, status: &str) -> Vec<Value> {
    let mut blocks = card_parts(card);
    blocks.push(json!({"type": "context", "elements": [{"type": "plain_text", "text": status, "emoji": false}]}));
    blocks
}

/// 알림 미리보기·블록이 거절됐을 때 쓰는 글자만 판
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
            title: "일정 삭제".into(),
            lines: vec!["대상: 산책 <!channel>".into(), "되돌릴 수 없어요".into()],
        }
    }

    #[test]
    fn broadcast_mentions_are_neutralised_but_user_markup_is_left_alone() {
        assert_eq!(
            neutralize("hi <!channel> and <!here> <!subteam^S1> <@U1>"),
            "hi &lt;!channel> and &lt;!here> &lt;!subteam^S1> <@U1>"
        );
        let block = markdown_block("<!everyone> 안녕");
        assert_eq!(block["text"], "&lt;!everyone> 안녕");
        // 본문은 자르지 않는다: 한도에 들어가는지는 `fits_block`이 알려 주고, 알림 멘션을 바꾼 길이로 센다
        assert_eq!(
            markdown_block(&"x".repeat(MARKDOWN_LIMIT))["text"]
                .as_str()
                .unwrap()
                .len(),
            MARKDOWN_LIMIT
        );
        assert!(fits_block(&"x".repeat(MARKDOWN_LIMIT)));
        assert!(!fits_block(&"x".repeat(MARKDOWN_LIMIT + 1)));
        // `<!`는 `&lt;!`로 늘어나므로 원문이 한도 안이어도 바꾼 글이 넘으면 담지 못한다
        assert!(!fits_block(&"<!".repeat(MARKDOWN_LIMIT / 2)));
    }

    #[test]
    fn cards_use_plain_text_and_carry_the_nonce_on_both_buttons() {
        let blocks = card_blocks(&card());
        assert_eq!(blocks[0]["type"], "header");
        // 줄은 plain_text라 `<!channel>`이 멘션으로 해석되지 않는다
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
        let resolved = resolved_blocks(&card(), "승인됨");
        assert!(resolved.iter().all(|b| b["type"] != "actions"));
        assert_eq!(resolved.last().unwrap()["elements"][0]["text"], "승인됨");
    }

    #[test]
    fn a_card_that_would_be_cut_is_not_sent_with_buttons() {
        assert!(card_fits(&card()));
        let mut long_title = card();
        long_title.title = "가".repeat(TITLE_LIMIT + 1);
        assert!(!card_fits(&long_title));
        let mut long_body = card();
        long_body.lines = vec!["가".repeat(500); 7];
        assert!(!card_fits(&long_body));
        assert_eq!(
            card_text(&card()),
            "일정 삭제\n대상: 산책 <!channel>\n되돌릴 수 없어요"
        );
    }
}
