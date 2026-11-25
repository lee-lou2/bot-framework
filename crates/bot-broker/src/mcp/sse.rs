//! Server-Sent Events 해석기. 응답 본문 조각을 받아 완성된 이벤트의 `data`만 돌려준다(MCP는 `data`에 JSON-RPC 메시지 하나를 싣는다).
//!
//! 규칙(WHATWG SSE): 줄은 `\n`·`\r\n`·`\r`로 끝나고, `:`로 시작하는 줄은 주석, 빈 줄이 이벤트를 끝낸다. 한 이벤트의 `data` 줄은 `\n`으로 잇는다.
//! `event`·`id`·`retry`는 읽되 쓰지 않는다(이 판의 MCP는 재개를 지원하지 않는다). 첫 글자의 BOM은 버린다.
//! 끝나지 않은 이벤트(빈 줄 없이 연결이 끝남)는 내보내지 않는다. 이벤트 하나의 크기에 한도가 있다.
#[derive(Debug, Eq, PartialEq)]
pub enum SseError {
    /// 이벤트 하나가 한도를 넘었다.
    TooLarge,
    /// UTF-8이 아닌 줄이 있다.
    Text,
}

#[derive(Debug)]
pub struct Events {
    /// 아직 줄이 끝나지 않은 바이트
    pending: Vec<u8>,
    /// 지금 모으는 이벤트의 `data`(줄 사이는 `\n`)
    data: Option<String>,
    max_event: usize,
    started: bool,
}

impl Events {
    pub fn new(max_event: usize) -> Self {
        Self {
            pending: Vec::new(),
            data: None,
            max_event,
            started: false,
        }
    }

    /// 조각을 먹고 완성된 이벤트의 `data`들을 돌려준다.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, SseError> {
        self.pending.extend_from_slice(chunk);
        if self.pending.len() > self.max_event.saturating_mul(2).max(4096) {
            // 줄바꿈 없이 이어지는 거대한 줄(이벤트 한도의 두 배까지만 쥔다)
            return Err(SseError::TooLarge);
        }
        let mut out = Vec::new();
        let mut start = 0;
        let mut i = 0;
        while i < self.pending.len() {
            let byte = self.pending[i];
            let end = match byte {
                b'\n' => Some((i, i + 1)),
                // `\r`이 조각 끝이면 다음 바이트가 `\n`인지 모르므로 더 받은 뒤에 끝낸다
                b'\r' if i + 1 < self.pending.len() => {
                    let next = if self.pending[i + 1] == b'\n' {
                        i + 2
                    } else {
                        i + 1
                    };
                    Some((i, next))
                }
                _ => None,
            };
            match end {
                Some((line_end, next)) => {
                    let line = self.pending[start..line_end].to_vec();
                    self.line(&line, &mut out)?;
                    start = next;
                    i = next;
                }
                None => i += 1,
            }
        }
        self.pending.drain(..start);
        Ok(out)
    }

    fn line(&mut self, raw: &[u8], out: &mut Vec<String>) -> Result<(), SseError> {
        let mut text = std::str::from_utf8(raw).map_err(|_| SseError::Text)?;
        if !self.started {
            self.started = true;
            text = text.strip_prefix('\u{feff}').unwrap_or(text);
        }
        if text.is_empty() {
            if let Some(data) = self.data.take() {
                out.push(data);
            }
            return Ok(());
        }
        if text.starts_with(':') {
            return Ok(());
        }
        let (name, value) = match text.split_once(':') {
            Some((name, value)) => (name, value.strip_prefix(' ').unwrap_or(value)),
            None => (text, ""),
        };
        if name == "data" {
            match &mut self.data {
                // 한 이벤트의 `data` 줄은 `\n`으로 잇는다(빈 `data:` 줄도 한 줄이다)
                Some(data) => {
                    if data.len() + value.len() + 1 > self.max_event {
                        return Err(SseError::TooLarge);
                    }
                    data.push('\n');
                    data.push_str(value);
                }
                None => {
                    if value.len() > self.max_event {
                        return Err(SseError::TooLarge);
                    }
                    self.data = Some(value.to_owned());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(chunks: &[&[u8]]) -> Vec<String> {
        let mut events = Events::new(1 << 20);
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(events.push(chunk).unwrap());
        }
        out
    }

    #[test]
    fn events_end_on_a_blank_line_and_split_chunks_do_not_matter() {
        let text = b"event: message\ndata: {\"a\":1}\n\ndata: second\n\n";
        assert_eq!(all(&[text]), ["{\"a\":1}", "second"]);
        // 어디서 잘라도 같다(바이트 하나씩 포함)
        let bytes: Vec<&[u8]> = text.chunks(1).collect();
        assert_eq!(all(&bytes), ["{\"a\":1}", "second"]);
        for cut in 1..text.len() {
            assert_eq!(
                all(&[&text[..cut], &text[cut..]]),
                ["{\"a\":1}", "second"],
                "{cut}"
            );
        }
    }

    #[test]
    fn line_endings_comments_multiline_data_and_the_bom() {
        assert_eq!(
            all(&[b"data: a\r\n\r\ndata: b\r\rdata: c\n\n"]),
            ["a", "b", "c"]
        );
        assert_eq!(all(&[b": keep-alive\n\n:\r\ndata: x\n\n"]), ["x"]);
        assert_eq!(
            all(&[b"data: line1\ndata: line2\ndata:\ndata: line4\n\n"]),
            ["line1\nline2\n\nline4"]
        );
        assert_eq!(all(&[b"\xEF\xBB\xBFdata: bom\n\n"]), ["bom"]);
        // `\r\n`이 조각 사이에서 갈려도 한 줄바꿈이다
        assert_eq!(all(&[b"data: a\r", b"\n\r", b"\ndata: b\n\n"]), ["a", "b"]);
        // 값 앞 공백은 하나만 벗긴다. 필드 이름 뒤에 `:`가 없으면 값이 빈 글이다
        assert_eq!(all(&[b"data:  two\n\ndata\n\n"]), [" two", ""]);
    }

    #[test]
    fn unfinished_events_and_other_fields_are_never_emitted() {
        assert!(all(&[b"data: never ends\n"]).is_empty());
        assert!(all(&[b"id: 7\nretry: 100\nevent: message\n\n"]).is_empty());
        assert_eq!(all(&[b"id: 7\ndata: kept\n\n"]), ["kept"]);
    }

    #[test]
    fn size_limits_and_bad_text_are_errors() {
        let mut small = Events::new(8);
        assert_eq!(small.push(b"data: 123456789\n\n"), Err(SseError::TooLarge));
        let mut long_line = Events::new(8);
        assert_eq!(long_line.push(&vec![b'x'; 9000]), Err(SseError::TooLarge));
        let mut bad = Events::new(64);
        assert_eq!(bad.push(b"data: \xff\xfe\n\n"), Err(SseError::Text));
    }
}
