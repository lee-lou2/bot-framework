//! 터미널에 그대로 찍어도 안전한 글. 모델·도구가 만든 글에는 제어 문자가 섞일 수 있고, 터미널에서 그것은 글이 아니라 **명령**이다:
//! ESC 시퀀스(화면 지우기·색 바꾸기·창 제목·하이퍼링크·클립보드 쓰기 `ESC ] 52`), 8비트 C1 제어(U+0080–U+009F — CSI는 U+009B, OSC는 U+009D),
//! BEL·BS·CR(줄 덮어쓰기)·DEL·NUL. 줄바꿈(`\n`)과 탭(`\t`)만 남기고 나머지 제어 문자는 보이는 표기(`\u{1b}`)로 바꾼다 —
//! 지우지 않고 보이게 해서 무엇이 있었는지 읽을 수 있고, ESC가 없어진 시퀀스의 나머지(`]52;c;…`)는 평범한 글이라 터미널이 해석하지 않는다.
//! `\r\n`은 줄바꿈 하나로 본다(CR 바로 뒤의 LF가 줄을 끝내므로 아무것도 덮어쓰지 못한다).

/// 줄바꿈·탭을 뺀 제어 문자(C0·DEL·C1)를 `\u{..}` 글자로 바꾼 글.
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
            "안녕하세요",
            "한 줄\n둘째 줄\n",
            "열\t열\t열",
            "😀 emoji · 한자 漢字 · ñ · \u{a0}\u{2028}",
            "a\r\nb\r\n",
        ] {
            assert_eq!(plain(text), text.replace("\r\n", "\n"), "{text:?}");
        }
    }

    #[test]
    fn escape_sequences_lose_their_escape_and_become_readable_text() {
        // 클립보드 쓰기(OSC 52)는 BEL로도 ST(`ESC \`)로도 끝날 수 있다
        assert_eq!(plain("a\u{1b}]52;c;QUJD\u{7}b"), r"a\u{1b}]52;c;QUJD\u{7}b");
        assert_eq!(
            plain("a\u{1b}]52;c;QUJD\u{1b}\\b"),
            r"a\u{1b}]52;c;QUJD\u{1b}\b"
        );
        // 화면 지우기·색·커서 이동(CSI), 창 제목(OSC 0), 하이퍼링크(OSC 8), DCS·APC
        for sequence in [
            "\u{1b}[2J",
            "\u{1b}[31m",
            "\u{1b}[1;1H",
            "\u{1b}]0;제목\u{7}",
            "\u{1b}]8;;http://x\u{7}링크\u{1b}]8;;\u{7}",
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
        // U+009B는 CSI, U+009D는 OSC, U+0090은 DCS, U+0085는 NEL — UTF-8 모드의 터미널은 그대로 제어로 읽는다
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
        // 남은 제어 문자는 줄바꿈과 탭뿐이다(둘은 따로 지켜진다)
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
        assert_eq!(
            plain("승인했어요\r거절했어요"),
            r"승인했어요\u{d}거절했어요"
        );
        assert_eq!(plain("끝\r"), r"끝\u{d}");
        assert_eq!(plain("a\r\nb"), "a\nb");
        assert_eq!(plain("a\r\r\nb"), "a\\u{d}\nb");
        assert_eq!(plain("a\n\rb"), "a\n\\u{d}b");
    }
}
