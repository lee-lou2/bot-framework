//! 텍스트 파일 검사와 YAML front matter 분리.
use crate::issue::Issue;

/// 눈에 보이지 않거나 방향을 바꿔 읽는 사람을 속일 수 있는 문자(Trojan Source 계열)와 제어 문자.
/// 줄바꿈·탭·캐리지 리턴은 허용한다. ZWJ/ZWNJ(U+200C/D)는 이모지·일부 문자에 쓰여 허용한다.
fn forbidden(c: char) -> bool {
    match c {
        '\n' | '\t' | '\r' => false,
        '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => true,
        '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{feff}' => true,
        '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => true,
        _ => false,
    }
}

/// 바이트가 BOM 없는 UTF-8이고 금지 문자가 없는지 본다. 통과하면 문자열을 돌려준다.
pub fn check_text<'a>(path: &str, bytes: &'a [u8]) -> Result<&'a str, Issue> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(Issue::error(
            "text-bom",
            path,
            "UTF-8 BOM이 있어요. BOM 없이 저장하세요",
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|e| Issue::error("text-utf8", path, format!("UTF-8이 아니에요: {e}")))?;
    if let Some((line, c)) = find_forbidden(text) {
        return Err(Issue::error(
            "text-control",
            path,
            format!(
                "허용하지 않는 제어·보이지 않는 문자 U+{:04X}가 있어요",
                c as u32
            ),
        )
        .at(format!("줄 {line}")));
    }
    Ok(text)
}

/// 문자열에 허용하지 않는 문자가 있는가(YAML 이스케이프로 만든 값 검사용)
pub fn has_forbidden(text: &str) -> bool {
    text.chars().any(forbidden)
}

fn find_forbidden(text: &str) -> Option<(usize, char)> {
    let mut line = 1;
    for c in text.chars() {
        if c == '\n' {
            line += 1;
        }
        if forbidden(c) {
            return Some((line, c));
        }
    }
    None
}

/// 이름 한 조각(파일·폴더)에도 같은 문자 규칙을 쓴다.
pub fn name_is_clean(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.chars().any(|c| forbidden(c) || c == '\\' || c == '/')
        && name == name.trim()
        && !name.ends_with('.')
}

#[derive(Debug)]
pub struct FrontMatter<'a> {
    /// `---` 줄 사이의 YAML 원문(없으면 None)
    pub yaml: Option<&'a str>,
    /// front matter 뒤 본문
    pub body: &'a str,
}

/// 첫 줄이 정확히 `---`이면 다음 `---` 줄까지를 front matter로 본다.
pub fn split_front_matter<'a>(path: &str, text: &'a str) -> Result<FrontMatter<'a>, Issue> {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Ok(FrontMatter {
            yaml: None,
            body: text,
        });
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok(FrontMatter {
                yaml: Some(&rest[..offset]),
                body: &rest[offset + line.len()..],
            });
        }
        offset += line.len();
    }
    Err(Issue::error(
        "front-matter-unclosed",
        path,
        "맨 위 `---`로 연 front matter를 닫는 `---` 줄이 없어요",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bom_control_and_bidi() {
        assert_eq!(
            check_text("a.md", b"\xef\xbb\xbfhi").unwrap_err().code,
            "text-bom"
        );
        assert_eq!(
            check_text("a.md", b"a\x00b").unwrap_err().code,
            "text-control"
        );
        let bidi = "a\u{202e}b".as_bytes();
        assert_eq!(check_text("a.md", bidi).unwrap_err().code, "text-control");
        assert_eq!(
            check_text("a.md", b"\xff\xfe").unwrap_err().code,
            "text-utf8"
        );
        assert!(check_text("a.md", "한글\r\n탭\t".as_bytes()).is_ok());
    }

    #[test]
    fn splits_front_matter() {
        let fm = split_front_matter("a.md", "---\nmodel: x\n---\n본문\n").unwrap();
        assert_eq!(fm.yaml, Some("model: x\n"));
        assert_eq!(fm.body, "본문\n");
        let none = split_front_matter("a.md", "본문\n---\n").unwrap();
        assert!(none.yaml.is_none());
        assert_eq!(
            split_front_matter("a.md", "---\nmodel: x\n")
                .unwrap_err()
                .code,
            "front-matter-unclosed"
        );
    }

    #[test]
    fn names() {
        assert!(name_is_clean("회의 규칙.md"));
        assert!(!name_is_clean(" a"));
        assert!(!name_is_clean("a."));
        assert!(!name_is_clean("a\\b"));
    }

    #[test]
    fn random_text_never_panics() {
        use crate::testrand::{Rng, yaml_soup};
        let mut rng = Rng::new(0x7e57);
        for _ in 0..20_000 {
            let text = format!(
                "{}{}",
                if rng.below(2) == 0 { "---\n" } else { "" },
                yaml_soup(&mut rng)
            );
            let _ = split_front_matter("fuzz.md", &text);
            let _ = check_text("fuzz.md", text.as_bytes());
            let _ = check_text("fuzz.md", &text.as_bytes()[..rng.below(text.len() + 1)]);
        }
    }
}
