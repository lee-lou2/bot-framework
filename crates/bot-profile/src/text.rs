//! UTF-8 text validation (rejecting BOM, control, and bidi chars) and YAML front matter splitting.
use crate::issue::Issue;

/// Rejects control and Trojan Source invisible/bidi chars; allows `\n`, `\t`, `\r`, and ZWJ/ZWNJ (U+200C/D).
fn forbidden(c: char) -> bool {
    match c {
        '\n' | '\t' | '\r' => false,
        '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => true,
        '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{feff}' => true,
        '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => true,
        _ => false,
    }
}

pub fn check_text<'a>(path: &str, bytes: &'a [u8]) -> Result<&'a str, Issue> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(Issue::error(
            "text-bom",
            path,
            "UTF-8 BOM is not allowed; save without BOM",
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|e| Issue::error("text-utf8", path, format!("Invalid UTF-8: {e}")))?;
    if let Some((line, c)) = find_forbidden(text) {
        return Err(Issue::error(
            "text-control",
            path,
            format!(
                "Disallowed control or invisible character U+{:04X}",
                c as u32
            ),
        )
        .at(format!("line {line}")));
    }
    Ok(text)
}

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

pub fn name_is_clean(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.chars().any(|c| forbidden(c) || c == '\\' || c == '/')
        && name == name.trim()
        && !name.ends_with('.')
}

#[derive(Debug)]
pub struct FrontMatter<'a> {
    pub yaml: Option<&'a str>,
    pub body: &'a str,
}

/// Splits leading `---` front matter; bare `---` at EOF stays plain text here for V1 `AGENT.md` compatibility.
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
        "Front matter opened with `---` is missing a closing `---` line",
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
        assert!(check_text("a.md", "utf8 ★\r\ntab\t".as_bytes()).is_ok());
    }

    #[test]
    fn splits_front_matter() {
        let fm = split_front_matter("a.md", "---\nmodel: x\n---\nbody\n").unwrap();
        assert_eq!(fm.yaml, Some("model: x\n"));
        assert_eq!(fm.body, "body\n");
        let none = split_front_matter("a.md", "body\n---\n").unwrap();
        assert!(none.yaml.is_none());
        assert_eq!(
            split_front_matter("a.md", "---\nmodel: x\n")
                .unwrap_err()
                .code,
            "front-matter-unclosed"
        );
        for bare in ["---", "---\r"] {
            let plain = split_front_matter("a.md", bare).unwrap();
            assert!(plain.yaml.is_none(), "{bare:?}");
            assert_eq!(plain.body, bare, "{bare:?}");
        }
    }

    #[test]
    fn names() {
        assert!(name_is_clean("meeting notes.md"));
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
