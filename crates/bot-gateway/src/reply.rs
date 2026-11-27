//! Strict parser and size enforcement for outbox reply payloads (`text`, `attachments`, `approval`).
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
        return Err("approval must be an object");
    };
    if map
        .keys()
        .any(|k| !matches!(k.as_str(), "nonce" | "title" | "lines"))
    {
        return Err("approval has unknown field");
    }
    let nonce = map.get("nonce").and_then(Value::as_str).unwrap_or_default();
    if nonce.is_empty() || nonce.len() > NONCE_BYTES || !nonce.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err("invalid approval.nonce");
    }
    let title = map.get("title").and_then(Value::as_str).unwrap_or_default();
    if title.trim().is_empty() || title.chars().count() > TITLE_CHARS || !clean(title) {
        return Err("invalid approval.title");
    }
    let lines = map
        .get("lines")
        .and_then(Value::as_array)
        .ok_or("approval.lines is required")?;
    if lines.len() > CARD_LINES {
        return Err("too many approval.lines");
    }
    let lines = lines
        .iter()
        .map(|line| {
            line.as_str()
                .filter(|l| l.chars().count() <= LINE_CHARS && clean(l))
                .map(str::to_owned)
                .ok_or("invalid approval.lines entry")
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
        return Err("attachment must be an object");
    };
    if map
        .keys()
        .any(|k| !matches!(k.as_str(), "name" | "type" | "data"))
    {
        return Err("attachment has unknown field");
    }
    let get = |key: &str| map.get(key).and_then(Value::as_str).unwrap_or_default();
    let name = get("name");
    let safe_name = !name.is_empty()
        && name.len() <= 255
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control);
    if !safe_name {
        return Err("invalid attachment name");
    }
    let mime = get("type");
    if mime.is_empty()
        || mime.len() > 100
        || !mime
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'/' | b'-'))
    {
        return Err("invalid attachment type");
    }
    let data = STANDARD
        .decode(get("data"))
        .map_err(|_| "attachment data is not valid base64")?;
    Ok(Attachment {
        name: name.to_owned(),
        mime: mime.to_owned(),
        data,
    })
}

impl ReplyBody {
    pub fn parse(payload: &JsonPayload) -> Result<Self, &'static str> {
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&payload.0) else {
            return Err("reply body must be a JSON object");
        };
        let map: Map<String, Value> = map;
        if map
            .keys()
            .any(|k| !matches!(k.as_str(), "text" | "attachments" | "approval"))
        {
            return Err("reply body has unknown field");
        }
        let text = match map.get("text") {
            None => None,
            Some(Value::String(text))
                if text.chars().count() <= TEXT_LIMIT && !text.trim().is_empty() =>
            {
                Some(text.clone())
            }
            Some(_) => return Err("invalid text"),
        };
        let attachments = match map.get("attachments") {
            None => Vec::new(),
            Some(Value::Array(list)) if list.len() <= ATTACHMENT_COUNT => list
                .iter()
                .map(parse_attachment)
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err("invalid attachments"),
        };
        if attachments.iter().map(|a| a.data.len()).sum::<usize>() > ATTACHMENT_BYTES {
            return Err("attachments total size exceeds limit");
        }
        let approval = map.get("approval").map(parse_card).transpose()?;
        if text.is_none() && attachments.is_empty() && approval.is_none() {
            return Err("at least one of text, attachments, or approval is required");
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
        let body = parse(json!({"text": "hello"})).unwrap();
        assert_eq!(body.text.as_deref(), Some("hello"));
        let body = parse(
            json!({"attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]}),
        )
        .unwrap();
        assert_eq!(body.attachments[0].data, b"hi");
        let body = parse(json!({"text": "t", "approval": {"nonce": "n-1", "title": "Delete approval", "lines": ["Target: Walk"]}})).unwrap();
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
            ("text too long", json!({"text": "★".repeat(TEXT_LIMIT + 1)})),
            ("blank text", json!({"text": "  "})),
            ("empty object", json!({})),
            ("unknown field", json!({"text": "x", "html": "<b>"})),
            (
                "9 attachments",
                json!({"attachments": vec![json!({"name": "a", "type": "a/b", "data": ""}); 9]}),
            ),
            (
                "attachments total size",
                json!({"attachments": vec![json!({"name": "a", "type": "a/b", "data": STANDARD.encode(vec![0u8; ATTACHMENT_BYTES / 2 + 1])}); 2]}),
            ),
            (
                "attachment path traversal",
                json!({"attachments": [{"name": "../x", "type": "a/b", "data": ""}]}),
            ),
            (
                "attachment bad base64",
                json!({"attachments": [{"name": "a", "type": "a/b", "data": "***"}]}),
            ),
            (
                "attachment bad mime",
                json!({"attachments": [{"name": "a", "type": "a b", "data": ""}]}),
            ),
            (
                "attachment unknown field",
                json!({"attachments": [{"name": "a", "type": "a/b", "data": "", "url": "x"}]}),
            ),
            (
                "21 card lines",
                card(json!({"lines": vec!["x"; CARD_LINES + 1]})),
            ),
            ("card nonce whitespace", card(json!({"nonce": "a b"}))),
            ("card empty title", card(json!({"title": ""}))),
            (
                "card title length",
                card(json!({"title": "★".repeat(TITLE_CHARS + 1)})),
            ),
            (
                "card line length",
                card(json!({"lines": ["★".repeat(LINE_CHARS + 1)]})),
            ),
            ("card unknown field", card(json!({"buttons": []}))),
        ] {
            assert!(parse(body).is_err(), "{name}");
        }
        assert!(ReplyBody::parse(&JsonPayload("[1]".into())).is_err());
        assert!(ReplyBody::parse(&JsonPayload("nope".into())).is_err());
        assert!(parse(json!({"text": "★".repeat(TEXT_LIMIT)})).is_ok());
        assert!(parse(card(json!({"lines": vec!["x"; CARD_LINES]}))).is_ok());
    }
}
