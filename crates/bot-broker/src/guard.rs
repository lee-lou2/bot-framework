//! 출력 자격 가드. 모델에게 돌려주는 결과·회신·로그에 자격이 섞이면 막거나 가린다.
//! 코드로 강제하는 것만 둔다. 판단이 필요한 규칙은 에이전트 지침에 둔다.
//!
//! 두 가지 검사가 있다.
//! - 토큰 모양: 널리 쓰는 접두사 뒤로 영숫자가 길게 이어진 글(값을 몰라도 잡는다).
//! - 선언 자격: 호스트가 이번 호출에 실제로 넣은 값의 바이트(짧은 비밀번호도 잡는다).

use std::ops::Range;

/// 가림 표시
pub const MASK: &str = "<가림>";

/// 로그 가림과 일반 부분 일치의 하한. 호스트가 선언한 자격의 출력 검사에는 적용하지 않는다.
const MIN_SECRET_LEN: usize = 12;

/// 토큰 모양: (접두사, 접두사 뒤로 이어져야 하는 영숫자·`-`의 최소 길이)
const SHAPES: [(&str, usize); 12] = [
    ("xoxb-", 10),
    ("xoxp-", 10),
    ("xoxa-", 10),
    ("xoxs-", 10),
    ("xapp-", 10),
    // `github_pat_` 본문은 `_`로 끊겨 첫 마디(22자)만 센다.
    ("ghp_", 30),
    ("gho_", 30),
    ("ghu_", 30),
    ("ghs_", 30),
    ("ghr_", 30),
    ("github_pat_", 20),
    ("sk-", 20),
];

/// `prefix` 뒤에 영숫자·`-`가 `min`자 이상 이어진 자리를 앞에서부터 찾는다(접두사 포함, 이어진 끝까지).
fn shapes<'a>(
    text: &'a str,
    prefix: &'a str,
    min: usize,
) -> impl Iterator<Item = Range<usize>> + 'a {
    let mut from = 0;
    std::iter::from_fn(move || {
        while let Some(i) = text[from..].find(prefix) {
            let begin = from + i;
            let start = begin + prefix.len();
            let run = text[start..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .unwrap_or(text.len() - start);
            if run >= min {
                from = start + run;
                return Some(begin..from);
            }
            from = start;
        }
        None
    })
}

/// 토큰 모양 또는 12바이트 이상인 비밀 값을 찾는다.
pub fn leaks_secret(text: &str, secrets: &[String]) -> bool {
    SHAPES
        .iter()
        .any(|(prefix, min)| shapes(text, prefix, *min).next().is_some())
        || secrets
            .iter()
            .any(|s| s.len() >= MIN_SECRET_LEN && text.contains(s.as_str()))
}

/// `needle`이 `haystack`에 들어 있는가. 실패 함수(KMP)로 최악에도 O(n+m)이다
/// (긴 출력과 긴 자격을 가진 적대적 입력이 검사를 늘어지게 하지 못한다).
pub(crate) fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let mut fail = vec![0usize; needle.len()];
    let mut k = 0;
    for i in 1..needle.len() {
        while k > 0 && needle[i] != needle[k] {
            k = fail[k - 1];
        }
        if needle[i] == needle[k] {
            k += 1;
        }
        fail[i] = k;
    }
    let mut k = 0;
    for &byte in haystack {
        while k > 0 && byte != needle[k] {
            k = fail[k - 1];
        }
        if byte == needle[k] {
            k += 1;
            if k == needle.len() {
                return true;
            }
        }
    }
    false
}

/// 글과 파일 원문에서 호스트가 선언한 자격의 바이트를 찾는다. 빈 값은 미설정이다.
pub fn contains_credential(data: impl AsRef<[u8]>, credentials: &[Vec<u8>]) -> bool {
    let bytes = data.as_ref();
    credentials.iter().any(|value| contains_bytes(bytes, value))
}

/// 결과와 첨부에는 토큰 모양과 모든 선언 자격을 검사한다. 비밀번호는 12바이트보다 짧을 수 있다.
pub fn blocks_output(text: &str, credentials: &[Vec<u8>]) -> bool {
    leaks_secret(text, &[]) || contains_credential(text, credentials)
}

/// 오류 문자열에 싣기 전에 토큰 모양을 가린다. 짧은 토큰도 단어째 가리는 보수형이다.
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        let w = word.trim_end();
        let hit = ["xoxb-", "xapp-", "xoxp-", "sk-"]
            .iter()
            .any(|p| w.contains(p));
        if hit {
            out.push_str(MASK);
            out.push_str(&word[w.len()..]);
        } else {
            out.push_str(word);
        }
    }
    out
}

/// 로그·추적·오류에 싣기 전에 비밀을 가린다. 프로필 비밀 값과 토큰 모양인 자리만 `<가림>`으로 바꾸고
/// 같은 문장의 나머지 글은 그대로 둔다. 가린 뒤에도 `leaks_secret`에 걸리면 글 전체를 가린다(마지막 안전장치).
pub fn redact(text: &str, secrets: &[String]) -> String {
    // 검사는 영숫자·`-`만 세지만(`github_pat_`은 첫 마디), 가릴 때는 `_`로 이어진 본문 끝까지 지워 꼬리가 남지 않게 한다.
    let mut spans: Vec<Range<usize>> = SHAPES
        .iter()
        .flat_map(|(prefix, min)| shapes(text, prefix, *min))
        .map(|r| {
            let tail = text[r.end..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .unwrap_or(text.len() - r.end);
            r.start..r.end + tail
        })
        .collect();
    for secret in secrets.iter().filter(|s| s.len() >= MIN_SECRET_LEN) {
        spans.extend(
            text.match_indices(secret.as_str())
                .map(|(at, found)| at..at + found.len()),
        );
    }
    spans.sort_unstable_by_key(|r| (r.start, r.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for span in merged {
        out.push_str(&text[at..span.start]);
        out.push_str(MASK);
        at = span.end;
    }
    out.push_str(&text[at..]);
    if leaks_secret(&out, secrets) {
        MASK.to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 토큰 모양 리터럴이 소스에 남지 않게(비밀 스캔 오탐 방지) 접두사를 실행 중에 이어 붙인다
    fn shaped(prefix: &str, rest: &str) -> String {
        format!("{prefix}-{rest}")
    }

    fn creds(values: &[&str]) -> Vec<Vec<u8>> {
        values.iter().map(|v| v.as_bytes().to_vec()).collect()
    }

    #[test]
    fn blocks_token_shapes_and_real_values() {
        assert!(leaks_secret(
            &format!("토큰은 {} 예요", shaped("xoxb", "1234567890-abcdefghij")),
            &[]
        ));
        assert!(leaks_secret(
            &format!("키 {}", shaped("xapp", "1-A0123456789-abcdef")),
            &[]
        ));
        assert!(leaks_secret(
            &shaped("sk", "3dabcdefghijklmnopqrstuvwxyz"),
            &[]
        ));
        assert!(leaks_secret(
            "값은 super-secret-value-123 입니다",
            &["super-secret-value-123".to_string()]
        ));
    }

    #[test]
    fn blocks_github_token_shapes() {
        let body = "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8";
        for prefix in ["ghp", "gho", "ghu", "ghs", "ghr"] {
            assert!(
                leaks_secret(&format!("토큰은 {prefix}_{body} 예요"), &[]),
                "{prefix}"
            );
        }
        assert!(leaks_secret(
            &format!("{}_{}_{}", "github_pat", "11AAAAAAA0abcdefghijkl", "rest"),
            &[]
        ));
    }

    #[test]
    fn allows_plain_explanations() {
        assert!(!leaks_secret("봇 토큰은 xoxb- 로 시작해요", &[]));
        assert!(!leaks_secret("sk- 접두사를 쓰는 키가 있어요", &[]));
        assert!(!leaks_secret(
            "GitHub 토큰은 ghp_ 나 github_pat_ 로 시작해요",
            &[]
        ));
        assert!(!leaks_secret("gho_짧음 은 토큰이 아니에요", &[]));
        assert!(!leaks_secret("평범한 답변", &["short".to_string()]));
    }

    #[test]
    fn output_checks_short_declared_passwords_and_ignores_unset_values() {
        let credentials = creds(&["", "pass1234"]);
        assert!(blocks_output("값(pass1234)입니다", &credentials));
        assert!(blocks_output(&shaped("xoxb", "1234567890"), &[]));
        assert!(!blocks_output("일반 값 1 / 1234.5678", &credentials));
    }

    #[test]
    fn credential_search_is_exact_and_linear_on_adversarial_input() {
        let needle = format!("{}b", "a".repeat(2000));
        let hay = "a".repeat(200_000);
        assert!(!contains_credential(&hay, &[needle.clone().into_bytes()]));
        assert!(contains_credential(
            format!("{hay}{needle}"),
            &[needle.into_bytes()]
        ));
        assert!(contains_credential(b"xxabcxx", &creds(&["abc"])));
        assert!(!contains_credential(b"xxabxcxx", &creds(&["abc"])));
        assert!(!contains_credential(b"", &creds(&["abc"])));
        assert!(contains_credential([0u8, 255, 7, 0], &[vec![255, 7]]));
    }

    #[test]
    fn scrub_hides_token_shapes() {
        let (bot, key) = (["xoxb", "-123-abc"].concat(), ["sk", "-3dabc"].concat());
        let s = scrub(&format!("fail {bot} end {key} more"));
        assert!(!s.contains(&bot) && !s.contains(&key));
        assert!(s.contains("fail") && s.contains("end"));
    }

    #[test]
    fn redact_hides_only_the_token_and_keeps_the_sentence() {
        let token = shaped("xoxb", "1234567890-abcdefghij");
        let out = redact(
            &format!("오류: 토큰 {token} 으로 호출했는데 거절됐어요"),
            &[],
        );
        assert_eq!(out, "오류: 토큰 <가림> 으로 호출했는데 거절됐어요");
        assert!(!leaks_secret(&out, &[]));
    }

    #[test]
    fn redact_hides_every_profile_secret_occurrence() {
        let secrets = ["super-secret-value-123".to_string()];
        let out = redact(
            "값은 super-secret-value-123 입니다 (다시: super-secret-value-123).",
            &secrets,
        );
        assert_eq!(out, "값은 <가림> 입니다 (다시: <가림>).");
        assert!(!leaks_secret(&out, &secrets));
    }

    #[test]
    fn redact_makes_one_mask_where_a_secret_sits_inside_a_token() {
        let token = shaped("xoxb", "1234567890-abcdefghij");
        let secrets = ["1234567890-abcdefghij".to_string()];
        assert_eq!(redact(&token, &secrets), "<가림>");
        // 이웃한 두 비밀도 하나로 이어 가린다
        let two = ["AAAAAAAAAAAA".to_string(), "BBBBBBBBBBBB".to_string()];
        assert_eq!(redact("AAAAAAAAAAAABBBBBBBBBBBB 끝", &two), "<가림> 끝");
    }

    #[test]
    fn redact_leaves_plain_text_and_short_values_alone() {
        for text in [
            "봇 토큰은 xoxb- 로 시작해요",
            "sk- 접두사를 쓰는 키가 있어요",
            "GitHub 토큰은 ghp_ 나 github_pat_ 로 시작해요",
            "평범한 답변",
        ] {
            assert_eq!(redact(text, &["short".to_string()]), text);
        }
    }

    #[test]
    fn redact_handles_github_shapes_and_text_around_multibyte() {
        let body = "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8";
        let pat = format!("{}_{}_{}", "github_pat", "11AAAAAAA0abcdefghijkl", "rest");
        for text in [
            format!("한글ghp_{body}한글"),
            format!("a {pat} b"),
            format!(
                "{}\n{}",
                shaped("sk", "3dabcdefghijklmnopqrstuvwxyz"),
                "다음 줄"
            ),
        ] {
            let out = redact(&text, &[]);
            assert!(out.contains(MASK), "{text}");
            assert!(!leaks_secret(&out, &[]), "{text}");
        }
        assert_eq!(
            redact(&format!("한글ghp_{body}한글"), &[]),
            "한글<가림>한글"
        );
        // 검사는 첫 마디만 세지만 가릴 때는 `_`로 이어진 나머지 본문까지 지운다
        assert_eq!(redact(&format!("a {pat} b"), &[]), "a <가림> b");
    }
}
