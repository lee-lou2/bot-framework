//! RFC 8785 JSON string escaping, hex encoding, and continuation-line formatting.

pub use bot_core::canon::json_quote;

/// Splits `value` across `max_bytes`-bounded lines (`"{label}: ..."` then `"  ..."`) without truncation.
/// Returns `None` if fewer than 4 bytes remain after the prefix (cannot fit a 4-byte UTF-8 scalar).
pub fn labeled_lines(label: &str, value: &str, max_bytes: usize) -> Option<Vec<String>> {
    let mut prefix = format!("{label}: ");
    let mut rest = value;
    let mut lines = Vec::new();
    loop {
        let room = max_bytes
            .checked_sub(prefix.len())
            .filter(|room| *room >= 4)?;
        let mut end = rest.len().min(room);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        lines.push(format!("{prefix}{}", &rest[..end]));
        rest = &rest[end..];
        if rest.is_empty() {
            return Some(lines);
        }
        prefix = "  ".to_owned();
    }
}

pub use bot_core::random::hex;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_follow_rfc_8785() {
        assert_eq!(json_quote("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(json_quote("line\nbreak\ttab"), "\"line\\nbreak\\ttab\"");
        assert_eq!(
            json_quote("\u{1}\u{1f}\u{8}\u{c}\r"),
            r#""\u0001\u001f\b\f\r""#
        );
        assert_eq!(json_quote("/ \u{7f} 😀"), "\"/ \u{7f} 😀\"");
    }

    #[test]
    fn quoted_text_never_contains_a_raw_newline() {
        let forged = "content\nRisk: read\nApprover: someone";
        assert!(!json_quote(forged).contains('\n'));
    }

    #[test]
    fn long_values_continue_on_following_lines_without_losing_a_byte() {
        let value = "€★♦♣♠".repeat(40); // 600 bytes (3-byte UTF-8 scalars)
        let lines = labeled_lines("Content", &value, 100).unwrap();
        assert!(lines.len() > 1 && lines.iter().all(|l| l.len() <= 100));
        assert!(
            lines[0].starts_with("Content: ") && lines[1..].iter().all(|l| l.starts_with("  "))
        );
        let joined: String = lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                if i == 0 {
                    &l["Content: ".len()..]
                } else {
                    &l[2..]
                }
            })
            .collect();
        assert_eq!(joined, value);
        assert_eq!(
            labeled_lines("a", "", 100).unwrap(),
            vec!["a: ".to_string()]
        );
        assert!(labeled_lines("longlabel".repeat(10).as_str(), "x", 20).is_none());
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0, 15, 16, 255]), "000f10ff");
    }
}
