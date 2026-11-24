//! 회신 본문(`Outgoing.body`) 해석. 형식·한도는 docs/reliability.md의 «게이트웨이 인계 계약»을 따른다:
//! text(최대 12,000자), attachments(최대 8개·디코딩 합계 8 MiB, 각 name/type/data), approval(nonce/title/lines 최대 20줄)
//! 중 하나 이상인 JSON 객체. 모르는 필드는 거절한다.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bot_core::command::JsonPayload;
use serde_json::{Map, Value};

pub const TEXT_LIMIT: usize = 12_000;
pub const ATTACHMENT_COUNT: usize = 8;
pub const ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
pub const CARD_LINES: usize = 20;
const LINE_CHARS: usize = 500;
const TITLE_CHARS: usize = 150;
const NONCE_BYTES: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// 브로커가 승인 원장과 같은 커밋에 만든 승인 카드
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Card {
    pub nonce: String,
    pub title: String,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReplyBody {
    pub text: Option<String>,
    pub attachments: Vec<Attachment>,
    pub approval: Option<Card>,
}

fn clean(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
}

pub fn parse_card(value: &Value) -> Result<Card, &'static str> {
    let Value::Object(map) = value else {
        return Err("approval은 객체여야 해요");
    };
    if map
        .keys()
        .any(|k| !matches!(k.as_str(), "nonce" | "title" | "lines"))
    {
        return Err("approval에 알 수 없는 필드가 있어요");
    }
    let nonce = map.get("nonce").and_then(Value::as_str).unwrap_or_default();
    if nonce.is_empty() || nonce.len() > NONCE_BYTES || !nonce.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err("approval.nonce가 올바르지 않아요");
    }
    let title = map.get("title").and_then(Value::as_str).unwrap_or_default();
    if title.trim().is_empty() || title.chars().count() > TITLE_CHARS || !clean(title) {
        return Err("approval.title이 올바르지 않아요");
    }
    let lines = map
        .get("lines")
        .and_then(Value::as_array)
        .ok_or("approval.lines가 필요해요")?;
    if lines.len() > CARD_LINES {
        return Err("approval.lines가 너무 많아요");
    }
    let lines = lines
        .iter()
        .map(|line| {
            line.as_str()
                .filter(|l| l.chars().count() <= LINE_CHARS && clean(l))
                .map(str::to_owned)
                .ok_or("approval.lines 항목이 올바르지 않아요")
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Card {
        nonce: nonce.to_owned(),
        title: title.to_owned(),
        lines,
    })
}

fn parse_attachment(value: &Value) -> Result<Attachment, &'static str> {
    let Value::Object(map) = value else {
        return Err("첨부는 객체여야 해요");
    };
    if map
        .keys()
        .any(|k| !matches!(k.as_str(), "name" | "type" | "data"))
    {
        return Err("첨부에 알 수 없는 필드가 있어요");
    }
    let get = |key: &str| map.get(key).and_then(Value::as_str).unwrap_or_default();
    let name = get("name");
    let safe_name = !name.is_empty()
        && name.len() <= 255
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control);
    if !safe_name {
        return Err("첨부 이름이 올바르지 않아요");
    }
    let mime = get("type");
    if mime.is_empty()
        || mime.len() > 100
        || !mime
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'/' | b'-'))
    {
        return Err("첨부 종류가 올바르지 않아요");
    }
    let data = STANDARD
        .decode(get("data"))
        .map_err(|_| "첨부 data가 base64가 아니에요")?;
    Ok(Attachment {
        name: name.to_owned(),
        mime: mime.to_owned(),
        data,
    })
}

impl ReplyBody {
    pub fn parse(payload: &JsonPayload) -> Result<Self, &'static str> {
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&payload.0) else {
            return Err("회신 본문은 JSON 객체여야 해요");
        };
        let map: Map<String, Value> = map;
        if map
            .keys()
            .any(|k| !matches!(k.as_str(), "text" | "attachments" | "approval"))
        {
            return Err("회신 본문에 알 수 없는 필드가 있어요");
        }
        let text = match map.get("text") {
            None => None,
            Some(Value::String(text))
                if text.chars().count() <= TEXT_LIMIT && !text.trim().is_empty() =>
            {
                Some(text.clone())
            }
            Some(_) => return Err("text가 올바르지 않아요"),
        };
        let attachments = match map.get("attachments") {
            None => Vec::new(),
            Some(Value::Array(list)) if list.len() <= ATTACHMENT_COUNT => list
                .iter()
                .map(parse_attachment)
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err("attachments가 올바르지 않아요"),
        };
        if attachments.iter().map(|a| a.data.len()).sum::<usize>() > ATTACHMENT_BYTES {
            return Err("첨부 합계가 한도를 넘어요");
        }
        let approval = map.get("approval").map(parse_card).transpose()?;
        if text.is_none() && attachments.is_empty() && approval.is_none() {
            return Err("text·attachments·approval 중 하나는 있어야 해요");
        }
        Ok(Self {
            text,
            attachments,
            approval,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value) -> Result<ReplyBody, &'static str> {
        ReplyBody::parse(&JsonPayload(value.to_string()))
    }

    #[test]
    fn reads_text_attachments_and_cards() {
        let body = parse(json!({"text": "안녕"})).unwrap();
        assert_eq!(body.text.as_deref(), Some("안녕"));
        let body = parse(
            json!({"attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]}),
        )
        .unwrap();
        assert_eq!(body.attachments[0].data, b"hi");
        let body = parse(json!({"text": "t", "approval": {"nonce": "n-1", "title": "삭제 승인", "lines": ["대상: 산책"]}})).unwrap();
        let card = body.approval.unwrap();
        assert_eq!((card.nonce.as_str(), card.lines.len()), ("n-1", 1));
    }

    #[test]
    fn enforces_every_documented_limit() {
        let card = |patch: Value| {
            let mut c = json!({"nonce": "n-1", "title": "t", "lines": ["x"]});
            for (k, v) in patch.as_object().unwrap() {
                c[k] = v.clone();
            }
            json!({"approval": c})
        };
        for (name, body) in [
            ("text 초과", json!({"text": "가".repeat(TEXT_LIMIT + 1)})),
            ("빈 text", json!({"text": "  "})),
            ("빈 객체", json!({})),
            ("알 수 없는 필드", json!({"text": "x", "html": "<b>"})),
            (
                "첨부 9개",
                json!({"attachments": vec![json!({"name": "a", "type": "a/b", "data": ""}); 9]}),
            ),
            (
                "첨부 합계 초과",
                json!({"attachments": vec![json!({"name": "a", "type": "a/b", "data": STANDARD.encode(vec![0u8; ATTACHMENT_BYTES / 2 + 1])}); 2]}),
            ),
            (
                "첨부 이름 경로",
                json!({"attachments": [{"name": "../x", "type": "a/b", "data": ""}]}),
            ),
            (
                "첨부 base64",
                json!({"attachments": [{"name": "a", "type": "a/b", "data": "***"}]}),
            ),
            (
                "첨부 종류",
                json!({"attachments": [{"name": "a", "type": "a b", "data": ""}]}),
            ),
            (
                "첨부 필드",
                json!({"attachments": [{"name": "a", "type": "a/b", "data": "", "url": "x"}]}),
            ),
            (
                "카드 줄 21개",
                card(json!({"lines": vec!["x"; CARD_LINES + 1]})),
            ),
            ("카드 nonce 공백", card(json!({"nonce": "a b"}))),
            ("카드 제목 없음", card(json!({"title": ""}))),
            (
                "카드 제목 길이",
                card(json!({"title": "가".repeat(TITLE_CHARS + 1)})),
            ),
            (
                "카드 줄 길이",
                card(json!({"lines": ["가".repeat(LINE_CHARS + 1)]})),
            ),
            ("카드 필드", card(json!({"buttons": []}))),
        ] {
            assert!(parse(body).is_err(), "{name}");
        }
        assert!(ReplyBody::parse(&JsonPayload("[1]".into())).is_err());
        assert!(ReplyBody::parse(&JsonPayload("nope".into())).is_err());
        // 정확히 한도인 경우는 받는다
        assert!(parse(json!({"text": "가".repeat(TEXT_LIMIT)})).is_ok());
        assert!(parse(card(json!({"lines": vec!["x"; CARD_LINES]}))).is_ok());
    }
}
