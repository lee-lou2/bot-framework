//! 모델 답의 인라인 첨부 블록(`<첨부 이름="근거.md">…</첨부>`) 분리와 게이트웨이 회신 문서 조립.
//!
//! 푸시 대화(Slack 채널·DM)의 최종 답에서 긴 표·쿼리·로그를 본문과 떼어내, 본문은 마크다운 메시지로,
//! 첨부는 `text/plain` 파일 업로드 회신(`{"attachments":[…]}`)으로 보내게 한다.
use crate::command::JsonPayload;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

const OPEN: &str = "<첨부";
const CLOSE: &str = "</첨부>";
pub const DEFAULT_NAME: &str = "근거.md";
/// 첨부 파일 하나의 글자 수 상한
pub const MAX_CHARS: usize = 300_000;
/// 첨부 파일 하나의 UTF-8 바이트 상한(base64 확장 뒤에도 저장소 문서 상한 1 MiB 안)
pub const MAX_UTF8_BYTES: usize = 512 * 1024;
/// 회신 문서 하나에 담는 최대 첨부 개수(저장소·게이트웨이 계약과 동일)
pub const MAX_PER_ANSWER: usize = 8;
/// 직렬화한 첨부 회신 문서 하나의 바이트 상한(저장소 `MAX_DOC_BYTES = 1 MiB` 여유 포함)
const MAX_ANSWER_JSON_BYTES: usize = 768 * 1024;
const EXTENSIONS: [&str; 8] = ["md", "txt", "csv", "tsv", "json", "sql", "log", "html"];

/// 본문에서 떼어낸 글자 첨부 하나: 안전한 파일 이름과 UTF-8 내용.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextAttachment {
    pub name: String,
    pub content: String,
}

/// 줄 맨 앞의 코드 펜스 열기(` ``` `·`~~~`) 표식 `(문자, 길이)`. 인라인 코드(`` ```a``` ``)는 열기가 아니다.
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

/// `rest`에서 대응하는 닫는 코드 펜스 줄을 찾아 그 줄 끝까지의 바이트 길이를 돌려준다.
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

/// 닫힌 코드 블록 바깥의 줄 맨 앞에서 시작하고 같은 줄에서 `>`로 닫히는 `<첨부` 태그 위치 `(태그 시작, '>' 위치)`.
/// 글 속에 인용한 태그(`` `<첨부>` `` 등)나 코드 블록 예시, 같은 줄에서 닫히지 않은 태그는 건드리지 않는다.
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

/// 태그 속성의 첫 따옴표 값을 파일 이름으로 쓴다.
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

/// 올릴 파일 이름: 안전한 글자만 남기고 허용한 글자 파일 확장자로 끝낸다(모르면 `.md`, 비면 `근거.md`).
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

/// 답의 첨부 블록(`<첨부 이름="근거.md">…</첨부>`)을 본문에서 떼어낸다 → `(본문, 첨부들)`.
/// 닫는 태그가 없으면 끝까지가 내용이고, 블록 밖의 글(뒤에 붙은 `※` 줄 등)은 본문 끝에 붙인다.
/// 본문이 비면 첨부 내용을 본문으로 올려 아무것도 잃지 않는다. 내용이 빈 블록은 버린다.
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

/// 첨부 내용을 문자 수·UTF-8 바이트 수 상한 안으로 자른다(경계 안전). 잘렸으면 끝에 `…`를 붙이고 `true`다.
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

/// 떼어낸 글자 첨부들을 게이트웨이 회신 문서(`{"attachments":[…]}`) 목록으로 묶는다.
/// 각 문서는 저장소 문서 상한(`1 MiB`)·문서당 첨부 상한(`8`개) 안에 맞추고, `type`은 예약 승인 카드도 읽을 수 있는 `text/plain`으로 고정한다.
/// 내용이 잘렸거나 답 상한(`max_answers`)으로 남긴 첨부가 있으면 `truncated`가 `true`다.
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
        let text = "**결론**\n- 하나\n\n※ 상세는 첨부 파일에 있어요.\n\n<첨부 이름=\"403 원인/근거.md\">\n## 실측\n- 9,180건\n</첨부>";
        let (body, files) = split(text);
        assert_eq!(body, "**결론**\n- 하나\n\n※ 상세는 첨부 파일에 있어요.");
        assert_eq!(files, [file("403-원인-근거.md", "## 실측\n- 9,180건")]);
    }

    #[test]
    fn text_without_a_block_quoted_tag_or_code_example_is_left_alone() {
        let fenced = "사용법:\n```markdown\n<첨부 이름=\"예시.md\">\n내용\n</첨부>\n```";
        for text in [
            "그냥 답",
            "태그는 `<첨부>`처럼 써요",
            "앞 <첨부 이름=\"x.md\">중간</첨부> 뒤",
            "<첨부파일>은 다른 말이에요",
            "본문\n<첨부 이름=\"닫히지않음\n다음 줄 a > b",
            fenced,
        ] {
            assert_eq!(split(text), (text.to_owned(), Vec::new()), "{text}");
        }
    }

    #[test]
    fn code_blocks_before_and_inside_an_attachment_work_together() {
        let text = "요약\n```sql\nselect 1;\n```\n\n<첨부 이름=\"근거.md\">\n## 쿼리\n```sql\nselect 2;\n```\n</첨부>";
        let (body, files) = split(text);
        assert_eq!(body, "요약\n```sql\nselect 1;\n```");
        assert_eq!(files, [file("근거.md", "## 쿼리\n```sql\nselect 2;\n```")]);
    }

    #[test]
    fn attachment_edge_cases_never_lose_or_duplicate_text() {
        // 닫는 태그가 없으면 끝까지가 내용이다
        assert_eq!(
            split("본문\n<첨부>\n길게 쓴 내용"),
            ("본문".to_owned(), vec![file("근거.md", "길게 쓴 내용")])
        );
        // 블록 뒤에 붙은 글은 본문 끝에 붙는다
        assert_eq!(
            split("본문\n<첨부>\n내용\n</첨부>\n※ 근거: 첨부"),
            (
                "본문\n\n※ 근거: 첨부".to_owned(),
                vec![file("근거.md", "내용")]
            )
        );
        // 블록이 여럿이어도 모두 나온다(영문 속성 이름·작은따옴표·둥근따옴표 포함)
        let (body, files) = split(
            "본문\n<첨부 이름=\"a.sql\">\nselect 1\n</첨부>\n<첨부 name='b.csv'>\n1,2\n</첨부>",
        );
        assert_eq!(
            (body.as_str(), files),
            (
                "본문",
                vec![file("a.sql", "select 1"), file("b.csv", "1,2")]
            )
        );
        // 본문이 없으면 첨부 내용이 본문이 된다
        assert_eq!(
            split("<첨부>\n내용뿐\n</첨부>"),
            ("내용뿐".to_owned(), Vec::new())
        );
        // 내용이 빈 블록은 파일로 만들지 않는다
        assert_eq!(
            split("본문\n<첨부>\n  \n</첨부>"),
            ("본문".to_owned(), Vec::new())
        );
        // 두 번째 태그가 같은 줄에 `>` 없이 깨져도 앞선 중간 글이 중복되지 않는다
        assert_eq!(
            split("본문\n<첨부>\n내용\n</첨부>\n중간\n<첨부 이름=\"깨짐"),
            (
                "본문\n\n중간\n<첨부 이름=\"깨짐".to_owned(),
                vec![file("근거.md", "내용")]
            )
        );
    }

    #[test]
    fn attachment_names_are_safe_and_end_in_a_text_extension() {
        assert_eq!(attach_name(""), "근거.md");
        assert_eq!(attach_name(" 이름=\"원인.md\""), "원인.md");
        assert_eq!(attach_name(" 이름=\"리포트.html\""), "리포트.html");
        assert_eq!(attach_name(" 이름='쿼리.SQL'"), "쿼리.SQL");
        assert_eq!(attach_name(" 이름=“상세 내용”"), "상세-내용.md");
        assert_eq!(
            attach_name(" 이름=\"../../etc/passwd\""),
            "etc-passwd.md",
            "경로 문자는 모두 사라진다"
        );
        assert_eq!(attach_name(" 이름=\"run.exe\""), "run.exe.md");
        assert_eq!(attach_name(" 이름=\"!!\""), "근거.md");
        assert_eq!(
            attach_name(&format!(" 이름=\"{}\"", "가".repeat(100)))
                .chars()
                .count(),
            63
        );
        // 4바이트 유니코드로 가득 차도 255바이트 한도 안이다
        assert!(file_name(&"𠀀".repeat(100)).len() <= 255);
    }

    #[test]
    fn truncation_respects_char_and_utf8_byte_boundaries() {
        assert_eq!(truncate_content("가나", 5, 100), ("가나".to_owned(), false));
        assert_eq!(
            truncate_content("가나다라", 2, 100),
            ("가나…".to_owned(), true)
        );
        let (out, cut) = truncate_content("가나다라", 100, 9);
        assert!(cut);
        assert_eq!(out, "가나…");
        assert!(out.len() <= 9);
    }

    #[test]
    fn packed_answers_fit_store_limits_and_pass_scheduled_delivery_draft() {
        let files = vec![
            file("근거.md", "## 상세\n- 1건"),
            file("query.sql", "select 1;"),
        ];
        let (packed, truncated) = pack_answers(&files, 15);
        assert!(!truncated);
        assert_eq!(packed.len(), 1);
        assert!(packed[0].0.len() <= MAX_ANSWER_JSON_BYTES);
        // 예약 전송 승인 카드 문서(`DeliveryDraft`)가 text/plain 첨부를 그대로 풀어 읽을 수 있다
        let mut answers = vec![JsonPayload(json!({"text": "결론이에요."}).to_string())];
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
        assert!(approved.contains("\"name\":\"근거.md\""));
        assert!(approved.contains("\"text\":\"## 상세\\n- 1건\""));
        assert!(approved.contains("\"name\":\"query.sql\""));
    }

    #[test]
    fn packing_batches_by_count_and_size_and_reports_truncation() {
        // 9개 첨부는 8개 + 1개 두 회신으로 나뉜다
        let nine: Vec<_> = (0..9).map(|i| file(&format!("f{i}.md"), "내용")).collect();
        let (packed, truncated) = pack_answers(&nine, 15);
        assert!(!truncated);
        assert_eq!(packed.len(), 2);

        // 큰 첨부 둘은 각각 1 MiB 아래 회신 문서로 나뉜다(문자·바이트 상한 안이라 잘리지 않는다)
        let big = "가".repeat(135_000);
        let (packed, truncated) = pack_answers(&[file("a.md", &big), file("b.md", &big)], 15);
        assert!(!truncated);
        assert_eq!(packed.len(), 2);
        assert!(packed.iter().all(|p| p.0.len() < (1 << 20)));

        // 답 상한을 넘으면 잘림을 알린다
        let (packed, truncated) = pack_answers(&[file("a.md", &big), file("b.md", &big)], 1);
        assert!(truncated);
        assert_eq!(packed.len(), 1);

        // 파일 하나가 문자·바이트 상한을 넘으면 잘리고 잘림을 알린다
        for huge in ["a".repeat(MAX_CHARS + 100), "가".repeat(200_000)] {
            let (packed, truncated) = pack_answers(&[file("huge.md", &huge)], 15);
            assert!(truncated);
            assert_eq!(packed.len(), 1);
            assert!(packed[0].0.len() < (1 << 20));
        }
    }
}
