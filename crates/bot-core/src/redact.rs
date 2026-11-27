//! 토큰·JWT 모양 탐지 단일 표. 브로커 출력 가드와 관리 추적 가림 장치가 같은 접두사·JWT 규칙을 공유한다.

use std::ops::Range;

/// 널리 쓰이는 서비스 토큰·키 접두사 단일 목록.
pub const SECRET_PREFIXES: &[&str] = &[
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "xoxr-",
    "xoxe-",
    "xapp-",
    "sk-",
    "sk_live_",
    "pk_live_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "AKIA",
    "ASIA",
    "AIza",
    "ya29.",
];

/// 접두사별 본문 규칙: `(접두사, 접두사 뒤 본문 최소 길이, 본문에 '.' 허용 여부)`.
const PREFIX_RULES: &[(&str, usize, bool)] = &[
    ("xoxb-", 10, false),
    ("xoxp-", 10, false),
    ("xoxa-", 10, false),
    ("xoxs-", 10, false),
    ("xoxr-", 10, false),
    ("xoxe-", 10, false),
    ("xapp-", 10, false),
    ("sk-", 20, false),
    ("sk_live_", 8, false),
    ("pk_live_", 8, false),
    ("ghp_", 30, false),
    ("gho_", 30, false),
    ("ghu_", 30, false),
    ("ghs_", 30, false),
    ("ghr_", 30, false),
    ("github_pat_", 20, false),
    ("AKIA", 12, false),
    ("ASIA", 12, false),
    ("AIza", 12, false),
    ("ya29.", 11, true),
];

/// 낱말이 알려진 비밀 토큰 접두사로 시작하는가.
pub fn has_secret_prefix(word: &str) -> bool {
    SECRET_PREFIXES.iter().any(|p| word.starts_with(p))
}

/// 낱말 하나가 JWT(`eyJ<헤더>.<본문>.<서명>`) 모양인가.
pub fn looks_like_jwt(word: &str) -> bool {
    word.starts_with("eyJ")
        && word.len() >= 30
        && word.matches('.').count() >= 2
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'='))
}

/// `begin` 앞 바이트가 ASCII 영숫자면 다른 낱말 내부이므로 토큰 시작으로 보지 않는다.
/// 한글 등 멀티바이트 문자 뒤(`한글ghp_...`)나 구분자(`=`, `"`, `_`, 공백) 뒤는 토큰 시작으로 본다.
fn is_token_boundary(text: &str, begin: usize) -> bool {
    begin == 0 || !text.as_bytes()[begin - 1].is_ascii_alphanumeric()
}

/// 자유 텍스트 안에서 알려진 접두사 토큰과 JWT의 바이트 범위를 모두 찾는다.
pub fn secret_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    for &(prefix, min_body, allow_dot) in PREFIX_RULES {
        let mut from = 0;
        while let Some(offset) = text[from..].find(prefix) {
            let begin = from + offset;
            let start = begin + prefix.len();
            if !is_token_boundary(text, begin) {
                from = start;
                continue;
            }
            let mut run = text[start..]
                .find(|c: char| {
                    !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || (allow_dot && c == '.'))
                })
                .unwrap_or(text.len() - start);
            if allow_dot {
                while run > 0 && text.as_bytes()[start + run - 1] == b'.' {
                    run -= 1;
                }
            }
            if run >= min_body {
                from = start + run;
                spans.push(begin..from);
            } else {
                from = start;
            }
        }
    }
    let mut from = 0;
    while let Some(offset) = text[from..].find("eyJ") {
        let begin = from + offset;
        if !is_token_boundary(text, begin) {
            from = begin + 3;
            continue;
        }
        let mut end = begin
            + text[begin..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '=')))
                .unwrap_or(text.len() - begin);
        while end > begin && text.as_bytes()[end - 1] == b'.' {
            end -= 1;
        }
        if looks_like_jwt(&text[begin..end]) {
            spans.push(begin..end);
            from = end;
        } else {
            from = begin + 3;
        }
    }
    spans
}

/// 오류 문자열 보수 가림용: 구분자(`-`·`_`·`.`)로 끝나는 알려진 토큰 접두사가 낱말 경계에서 시작하고
/// 뒤에 본문 글자가 한 자 이상 이어지는가(`ASIA`·`task-123` 같은 일반 낱말 오탐을 막는다).
pub fn has_delimited_secret_prefix(word: &str) -> bool {
    SECRET_PREFIXES
        .iter()
        .filter(|p| p.ends_with(['-', '_', '.']))
        .any(|prefix| {
            let mut from = 0;
            while let Some(offset) = word[from..].find(prefix) {
                let begin = from + offset;
                let start = begin + prefix.len();
                if is_token_boundary(word, begin)
                    && word[start..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphanumeric())
                {
                    return true;
                }
                from = start;
            }
            false
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_prefixes_and_rules_stay_in_exact_lockstep() {
        let rule_prefixes: Vec<&str> = PREFIX_RULES.iter().map(|(p, _, _)| *p).collect();
        assert_eq!(rule_prefixes.as_slice(), SECRET_PREFIXES);
    }

    #[test]
    fn detects_every_prefix_and_jwt_and_ignores_plain_words() {
        let long_body = "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8";
        for &prefix in SECRET_PREFIXES {
            let sample = format!("{prefix}{long_body}");
            assert!(has_secret_prefix(&sample), "{prefix}");
            let text = format!("한글{sample}끝");
            let spans = secret_spans(&text);
            assert_eq!(spans.len(), 1, "{prefix}");
            assert_eq!(&text[spans[0].clone()], sample);
        }
        let jwt = format!(
            "{}.{}.{}",
            "eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiJhYmMifQ", "c2lnbmF0dXJlLXZhbHVl"
        );
        assert!(looks_like_jwt(&jwt));
        let wrapped = format!("Bearer {jwt} done");
        let spans = secret_spans(&wrapped);
        assert_eq!(spans.len(), 1);
        assert_eq!(&wrapped[spans[0].clone()], jwt);
        // 낱말 중간에 우연히 낀 접두사(`task-...`, `EASTASIA...`)는 오탐하지 않는다.
        assert!(secret_spans("task-1234567890123456789012345").is_empty());
        assert!(secret_spans("EASTASIA123456789012345").is_empty());
        // `ya29.`·JWT 뒤의 문장 마침표는 토큰 범위에 삼키지 않는다.
        let oauth = format!("{}{}", "ya29.", "a0AfH6SMBx123456");
        let sent = format!("토큰 {oauth}. 끝");
        let spans = secret_spans(&sent);
        assert_eq!(spans.len(), 1);
        assert_eq!(&sent[spans[0].clone()], oauth);
        let jwt_sent = format!("Bearer {jwt}. 끝");
        let spans = secret_spans(&jwt_sent);
        assert_eq!(spans.len(), 1);
        assert_eq!(&jwt_sent[spans[0].clone()], jwt);
        assert!(!looks_like_jwt("eyJshort.a.b"));
        assert!(!looks_like_jwt("eyJno_dots_at_all_12345678901234567890"));
    }
}
