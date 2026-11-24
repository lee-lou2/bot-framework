//! 글 다루기: JSON 문자열 이스케이프(RFC 8785 규칙).

pub use bot_core::canon::json_quote;

/// 한 줄 상한(`max_bytes`) 안에서 값을 이어 쓴 줄들. 첫 줄은 `라벨: 값 조각`, 이어지는 줄은 공백 두 칸으로 시작한다.
/// 이어 붙이면 원래 값이 그대로 나온다(자르지 않는다). 한 글자도 담을 수 없는 상한이면 `None`이다.
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

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_follow_rfc_8785() {
        assert_eq!(json_quote("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(json_quote("줄\n바꿈\t탭"), "\"줄\\n바꿈\\t탭\"");
        assert_eq!(
            json_quote("\u{1}\u{1f}\u{8}\u{c}\r"),
            r#""\u0001\u001f\b\f\r""#
        );
        assert_eq!(json_quote("/ \u{7f} 😀"), "\"/ \u{7f} 😀\"");
    }

    #[test]
    fn quoted_text_never_contains_a_raw_newline() {
        let forged = "내용\n위험: read\n승인자: 누군가";
        assert!(!json_quote(forged).contains('\n'));
    }

    #[test]
    fn long_values_continue_on_following_lines_without_losing_a_byte() {
        let value = "가나다라마".repeat(40); // 600바이트
        let lines = labeled_lines("내용", &value, 100).unwrap();
        assert!(lines.len() > 1 && lines.iter().all(|l| l.len() <= 100));
        assert!(lines[0].starts_with("내용: ") && lines[1..].iter().all(|l| l.starts_with("  ")));
        let joined: String = lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                if i == 0 {
                    &l["내용: ".len()..]
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
        assert!(labeled_lines("긴라벨".repeat(10).as_str(), "x", 20).is_none());
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0, 15, 16, 255]), "000f10ff");
    }
}
