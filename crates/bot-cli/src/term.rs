//! Sanitizes untrusted text for terminal output by escaping C0, DEL, and C1 controls while keeping `\n` and `\t`.
// Strips `\r` only in `\r\n` pairs; bare `\r` and ESC/CSI/OSC/C1 controls become `\u{..}` so terminal emulators cannot execute them.

pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\t' => out.push(c),
            c if c.is_control() => out.extend(c.escape_unicode()),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_text_newlines_and_tabs_pass_through_unchanged() {
        for text in [
            "",
            "hello",
            "line one\nline two\n",
            "col\tcol\tcol",
            "😀 emoji · CJK 漢字 · ñ · \u{a0}\u{2028}",
            "a\r\nb\r\n",
        ] {
            assert_eq!(plain(text), text.replace("\r\n", "\n"), "{text:?}");
        }
    }

    #[test]
    fn escape_sequences_lose_their_escape_and_become_readable_text() {
        assert_eq!(plain("a\u{1b}]52;c;QUJD\u{7}b"), r"a\u{1b}]52;c;QUJD\u{7}b");
        assert_eq!(
            plain("a\u{1b}]52;c;QUJD\u{1b}\\b"),
            r"a\u{1b}]52;c;QUJD\u{1b}\b"
        );
        for sequence in [
            "\u{1b}[2J",
            "\u{1b}[31m",
            "\u{1b}[1;1H",
            "\u{1b}]0;title\u{7}",
            "\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}",
            "\u{1b}Pq\u{1b}\\",
            "\u{1b}_x\u{1b}\\",
        ] {
            let shown = plain(sequence);
            assert!(
                !shown.contains('\u{1b}') && !shown.contains('\u{7}'),
                "{shown}"
            );
            assert!(shown.starts_with(r"\u{1b}"), "{shown}");
        }
    }

    #[test]
    fn eight_bit_c1_controls_are_not_left_for_the_terminal_either() {
        assert_eq!(plain("\u{9b}31m"), r"\u{9b}31m");
        assert_eq!(plain("\u{9d}52;c;QUJD\u{9c}"), r"\u{9d}52;c;QUJD\u{9c}");
        assert_eq!(
            plain("\u{90}\u{85}\u{80}\u{9f}"),
            r"\u{90}\u{85}\u{80}\u{9f}"
        );
    }

    #[test]
    fn every_other_control_is_made_visible_and_none_survive() {
        let mut all = String::new();
        for code in (0u32..0x20).chain(0x7f..=0x9f) {
            all.push(char::from_u32(code).unwrap());
        }
        let shown = plain(&all);
        assert!(
            shown
                .chars()
                .filter(|c| c.is_control())
                .all(|c| c == '\n' || c == '\t'),
            "{shown:?}"
        );
        for visible in [
            r"\u{0}", r"\u{7}", r"\u{8}", r"\u{b}", r"\u{c}", r"\u{d}", r"\u{1b}", r"\u{7f}",
            r"\u{85}",
        ] {
            assert!(shown.contains(visible), "{visible}: {shown}");
        }
        assert!(shown.contains('\n') && shown.contains('\t'));
    }

    #[test]
    fn a_carriage_return_cannot_overwrite_a_line_but_crlf_is_one_line_break() {
        assert_eq!(plain("approved\rrejected"), r"approved\u{d}rejected");
        assert_eq!(plain("end\r"), r"end\u{d}");
        assert_eq!(plain("a\r\nb"), "a\nb");
        assert_eq!(plain("a\r\r\nb"), "a\\u{d}\nb");
        assert_eq!(plain("a\n\rb"), "a\n\\u{d}b");
    }
}
