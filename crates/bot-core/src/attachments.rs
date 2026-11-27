//! Inline attachment block (`<attachment name="attachment.md">...</attachment>`) splitting and gateway reply packing.
use crate::command::JsonPayload;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

const OPEN: &str = "<attachment";
const CLOSE: &str = "</attachment>";
pub const DEFAULT_NAME: &str = "attachment.md";
pub const MAX_CHARS: usize = 300_000;
/// Stays within the 1 MiB store document limit after base64 expansion.
pub const MAX_UTF8_BYTES: usize = 512 * 1024;
/// Matches the store and gateway per-reply attachment limit.
pub const MAX_PER_ANSWER: usize = 8;
/// Leaves headroom below the store's 1 MiB `MAX_DOC_BYTES` limit.
const MAX_ANSWER_JSON_BYTES: usize = 768 * 1024;
const EXTENSIONS: [&str; 8] = ["md", "txt", "csv", "tsv", "json", "sql", "log", "html"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextAttachment {
    pub name: String,
    pub content: String,
}

/// Leading fenced-code opener (` ``` ` or `~~~`); excludes inline code like `` ```a``` ``.
fn fence_prefix(trimmed: &str) -> Option<(char, usize)> {
    let ch = trimmed.chars().next()?;
    if !matches!(ch, '`' | '~') {
        return None;
    }
    let len = trimmed.chars().take_while(|&c| c == ch).count();
    if len < 3 {
        return None;
    }
    let info = &trimmed[len..];
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((ch, len))
}

fn find_closing_fence(rest: &str, (ch, min_len): (char, usize)) -> Option<usize> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let trimmed = line.trim_start();
        let len = trimmed.chars().take_while(|&c| c == ch).count();
        if len >= min_len && trimmed[len..].trim().is_empty() {
            return Some(offset);
        }
    }
    None
}

/// Finds the first line-leading `<attachment ...>` closed on the same line outside fenced code blocks.
fn find_open(text: &str) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    let mut lines = text.split_inclusive('\n');
    while let Some(line) = lines.next() {
        let line_offset = offset;
        offset += line.len();
        let trimmed = line.trim_start();
        if let Some(fence) = fence_prefix(trimmed)
            && let Some(end) = find_closing_fence(&text[offset..], fence)
        {
            let target = offset + end;
            while offset < target {
                let Some(skipped) = lines.next() else {
                    break;
                };
                offset += skipped.len();
            }
            continue;
        }
        if let Some(after) = trimmed.strip_prefix(OPEN)
            && after
                .chars()
                .next()
                .is_some_and(|c| c == '>' || c.is_whitespace())
            && let Some(gt) = after.find('>')
        {
            let leading = line.len() - trimmed.len();
            let start = line_offset + leading;
            let close_gt = start + OPEN.len() + gt;
            return Some((start, close_gt));
        }
    }
    None
}

/// Uses the first quoted attribute value as the filename.
fn attach_name(attrs: &str) -> String {
    let quoted = attrs.find(['"', '\'', '“']).and_then(|s| {
        let rest = &attrs[s..];
        let open = rest.chars().next()?;
        let close = if open == '“' { '”' } else { open };
        let body = &rest[open.len_utf8()..];
        body.find(close).map(|e| body[..e].to_owned())
    });
    file_name(&quoted.unwrap_or_default())
}

/// Sanitizes a filename to safe characters and an allowed text extension (default `.md`, fallback `DEFAULT_NAME`).
pub fn file_name(raw: &str) -> String {
    let clean: String = raw
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let clean: String = clean
        .trim_matches(|c| c == '.' || c == '-')
        .chars()
        .take(60)
        .collect();
    if clean.is_empty() {
        return DEFAULT_NAME.to_owned();
    }
    match clean.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty() && EXTENSIONS.contains(&ext.to_lowercase().as_str()) =>
        {
            clean
        }
        _ => format!("{clean}.md"),
    }
}

/// Splits inline `<attachment ...>...</attachment>` blocks from the reply body.
/// Unclosed blocks run to EOF; trailing text appends to the body; empty bodies promote attachment text so nothing is lost.
pub fn split(text: &str) -> (String, Vec<TextAttachment>) {
    let Some((start, close_gt)) = find_open(text) else {
        return (text.to_owned(), Vec::new());
    };
    let mut parts = Vec::new();
    let mut files = Vec::new();
    let mut rest = text;
    let mut next = Some((start, close_gt));
    while let Some((s, gt)) = next {
        parts.push(rest[..s].trim().to_owned());
        let attrs = &rest[s + OPEN.len()..gt];
        let inner = &rest[gt + 1..];
        let (content, remain) = match inner.find(CLOSE) {
            Some(end) => (&inner[..end], &inner[end + CLOSE.len()..]),
            None => (inner, ""),
        };
        files.push(TextAttachment {
            name: attach_name(attrs),
            content: content.trim().to_owned(),
        });
        rest = remain;
        next = find_open(rest);
    }
    parts.push(rest.trim().to_owned());
    files.retain(|f| !f.content.is_empty());
    let body = parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if body.is_empty() {
        let promoted = files
            .iter()
            .map(|f| f.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        return (promoted, Vec::new());
    }
    (body, files)
}

fn truncate_content(content: &str, max_chars: usize, max_bytes: usize) -> (String, bool) {
    if content.len() <= max_bytes && content.chars().count() <= max_chars {
        return (content.to_owned(), false);
    }
    const ELLIPSIS: char = '…';
    let byte_cap = max_bytes.saturating_sub(ELLIPSIS.len_utf8());
    let mut end = 0usize;
    for (count, (idx, ch)) in content.char_indices().enumerate() {
        let next = idx + ch.len_utf8();
        if count >= max_chars || next > byte_cap {
            break;
        }
        end = next;
    }
    let mut out = String::with_capacity(end + ELLIPSIS.len_utf8());
    out.push_str(&content[..end]);
    if out.len() + ELLIPSIS.len_utf8() <= max_bytes {
        out.push(ELLIPSIS);
    }
    (out, true)
}

/// Packs attachments into gateway `{"attachments":[...]}` payloads within store size/count limits.
/// Fixes `type` to `text/plain` so scheduled delivery approval cards can decode them.
pub fn pack_answers(files: &[TextAttachment], max_answers: usize) -> (Vec<JsonPayload>, bool) {
    if files.is_empty() {
        return (Vec::new(), false);
    }
    if max_answers == 0 {
        return (Vec::new(), true);
    }
    let mut answers = Vec::new();
    let mut batch: Vec<Value> = Vec::new();
    let mut batch_bytes = "{\"attachments\":[]}".len();
    let mut truncated = false;
    for file in files {
        let (content, clipped) = truncate_content(&file.content, MAX_CHARS, MAX_UTF8_BYTES);
        truncated |= clipped;
        let item = json!({
            "name": file.name,
            "type": "text/plain",
            "data": STANDARD.encode(content.as_bytes()),
        });
        let item_bytes = item.to_string().len() + usize::from(!batch.is_empty());
        if !batch.is_empty()
            && (batch.len() >= MAX_PER_ANSWER || batch_bytes + item_bytes > MAX_ANSWER_JSON_BYTES)
        {
            answers.push(JsonPayload(json!({ "attachments": batch }).to_string()));
            batch = Vec::new();
            batch_bytes = "{\"attachments\":[]}".len();
            if answers.len() >= max_answers {
                truncated = true;
                break;
            }
        }
        batch_bytes += item_bytes;
        batch.push(item);
    }
    if !batch.is_empty() {
        if answers.len() < max_answers {
            answers.push(JsonPayload(json!({ "attachments": batch }).to_string()));
        } else {
            truncated = true;
        }
    }
    (answers, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        id::{BindingId, Revision},
        schedule::{DeliveryDraft, DeliveryTarget},
        state::{Conversation, ConversationKind},
    };

    fn file(name: &str, content: &str) -> TextAttachment {
        TextAttachment {
            name: name.into(),
            content: content.into(),
        }
    }

    #[test]
    fn attachment_blocks_leave_a_short_body_and_become_files() {
        let text = "**Summary**\n- Item one\n\nSee the attached file for details.\n\n<attachment name=\"403 cause/details.md\">\n## Metrics\n- 9,180 rows\n</attachment>";
        let (body, files) = split(text);
        assert_eq!(
            body,
            "**Summary**\n- Item one\n\nSee the attached file for details."
        );
        assert_eq!(
            files,
            [file("403-cause-details.md", "## Metrics\n- 9,180 rows")]
        );
    }

    #[test]
    fn text_without_a_block_quoted_tag_or_code_example_is_left_alone() {
        let fenced =
            "Usage:\n```markdown\n<attachment name=\"example.md\">\nbody\n</attachment>\n```";
        for text in [
            "plain reply",
            "write `<attachment>` like this",
            "before <attachment name=\"x.md\">mid</attachment> after",
            "<attachments> is a different tag",
            "body\n<attachment name=\"unclosed\nnext line a > b",
            fenced,
        ] {
            assert_eq!(split(text), (text.to_owned(), Vec::new()), "{text}");
        }
    }

    #[test]
    fn code_blocks_before_and_inside_an_attachment_work_together() {
        let text = "Summary\n```sql\nselect 1;\n```\n\n<attachment name=\"attachment.md\">\n## Query\n```sql\nselect 2;\n```\n</attachment>";
        let (body, files) = split(text);
        assert_eq!(body, "Summary\n```sql\nselect 1;\n```");
        assert_eq!(
            files,
            [file("attachment.md", "## Query\n```sql\nselect 2;\n```")]
        );
    }

    #[test]
    fn attachment_edge_cases_never_lose_or_duplicate_text() {
        assert_eq!(
            split("body\n<attachment>\nlong text"),
            ("body".to_owned(), vec![file("attachment.md", "long text")])
        );
        assert_eq!(
            split("body\n<attachment>\ncontent\n</attachment>\nNote: see attachment"),
            (
                "body\n\nNote: see attachment".to_owned(),
                vec![file("attachment.md", "content")]
            )
        );
        let (body, files) = split(
            "body\n<attachment file=\"a.sql\">\nselect 1\n</attachment>\n<attachment name='b.csv'>\n1,2\n</attachment>",
        );
        assert_eq!(
            (body.as_str(), files),
            (
                "body",
                vec![file("a.sql", "select 1"), file("b.csv", "1,2")]
            )
        );
        assert_eq!(
            split("<attachment>\nonly content\n</attachment>"),
            ("only content".to_owned(), Vec::new())
        );
        assert_eq!(
            split("body\n<attachment>\n  \n</attachment>"),
            ("body".to_owned(), Vec::new())
        );
        assert_eq!(
            split("body\n<attachment>\ncontent\n</attachment>\nmiddle\n<attachment name=\"broken"),
            (
                "body\n\nmiddle\n<attachment name=\"broken".to_owned(),
                vec![file("attachment.md", "content")]
            )
        );
    }

    #[test]
    fn attachment_names_are_safe_and_end_in_a_text_extension() {
        assert_eq!(attach_name(""), "attachment.md");
        assert_eq!(attach_name(" name=\"cause.md\""), "cause.md");
        assert_eq!(attach_name(" name=\"report.html\""), "report.html");
        assert_eq!(attach_name(" name='query.SQL'"), "query.SQL");
        assert_eq!(attach_name(" name=“detailed notes”"), "detailed-notes.md");
        assert_eq!(
            attach_name(" name=\"../../etc/passwd\""),
            "etc-passwd.md",
            "path separators must be stripped"
        );
        assert_eq!(attach_name(" name=\"run.exe\""), "run.exe.md");
        assert_eq!(attach_name(" name=\"!!\""), "attachment.md");
        assert_eq!(
            attach_name(&format!(" name=\"{}\"", "é".repeat(100)))
                .chars()
                .count(),
            63
        );
        // Even 4-byte Unicode characters stay within the 255-byte filename limit.
        assert!(file_name(&"𠀀".repeat(100)).len() <= 255);
    }

    #[test]
    fn truncation_respects_char_and_utf8_byte_boundaries() {
        assert_eq!(truncate_content("€★", 5, 100), ("€★".to_owned(), false));
        assert_eq!(truncate_content("€★♦♠", 2, 100), ("€★…".to_owned(), true));
        let (out, cut) = truncate_content("€★♦♠", 100, 9);
        assert!(cut);
        assert_eq!(out, "€★…");
        assert!(out.len() <= 9);
    }

    #[test]
    fn packed_answers_fit_store_limits_and_pass_scheduled_delivery_draft() {
        let files = vec![
            file("attachment.md", "## Details\n- 1 row"),
            file("query.sql", "select 1;"),
        ];
        let (packed, truncated) = pack_answers(&files, 15);
        assert!(!truncated);
        assert_eq!(packed.len(), 1);
        assert!(packed[0].0.len() <= MAX_ANSWER_JSON_BYTES);
        let mut answers = vec![JsonPayload(json!({"text": "Summary."}).to_string())];
        answers.extend(packed);
        let draft = DeliveryDraft {
            activation: Revision("set-1".into()),
            target: DeliveryTarget {
                binding: BindingId("team".into()),
                conversation: Conversation {
                    kind: ConversationKind::Channel,
                    id: "T1/C1".into(),
                    thread: None,
                },
            },
            answers,
        };
        let approved = draft.approval_json().unwrap();
        assert!(approved.contains("\"name\":\"attachment.md\""));
        assert!(approved.contains("\"text\":\"## Details\\n- 1 row\""));
        assert!(approved.contains("\"name\":\"query.sql\""));
    }

    #[test]
    fn packing_batches_by_count_and_size_and_reports_truncation() {
        let nine: Vec<_> = (0..9)
            .map(|i| file(&format!("f{i}.md"), "content"))
            .collect();
        let (packed, truncated) = pack_answers(&nine, 15);
        assert!(!truncated);
        assert_eq!(packed.len(), 2);

        // Two large attachments within per-file limits split across two sub-1-MiB payloads without truncation.
        let big = "€".repeat(135_000);
        let (packed, truncated) = pack_answers(&[file("a.md", &big), file("b.md", &big)], 15);
        assert!(!truncated);
        assert_eq!(packed.len(), 2);
        assert!(packed.iter().all(|p| p.0.len() < (1 << 20)));

        let (packed, truncated) = pack_answers(&[file("a.md", &big), file("b.md", &big)], 1);
        assert!(truncated);
        assert_eq!(packed.len(), 1);

        for huge in ["a".repeat(MAX_CHARS + 100), "€".repeat(200_000)] {
            let (packed, truncated) = pack_answers(&[file("huge.md", &huge)], 15);
            assert!(truncated);
            assert_eq!(packed.len(), 1);
            assert!(packed[0].0.len() < (1 << 20));
        }
    }
}
