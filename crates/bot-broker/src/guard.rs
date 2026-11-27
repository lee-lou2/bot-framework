//! Output credential guard that blocks or redacts token shapes and declared credential values.

use std::ops::Range;

pub const MASK: &str = "<redacted>";

/// Lower bound for log redaction and substring secret scans; does not apply to declared call credentials.
const MIN_SECRET_LEN: usize = 12;

pub fn leaks_secret(text: &str, secrets: &[String]) -> bool {
    !bot_core::redact::secret_spans(text).is_empty()
        || secrets
            .iter()
            .any(|s| s.len() >= MIN_SECRET_LEN && text.contains(s.as_str()))
}

/// KMP substring search guaranteeing O(n+m) worst-case time on adversarial output and credential sizes.
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

/// Searches raw bytes for any declared credential value (empty values are treated as unset).
pub fn contains_credential(data: impl AsRef<[u8]>, credentials: &[Vec<u8>]) -> bool {
    let bytes = data.as_ref();
    credentials.iter().any(|value| contains_bytes(bytes, value))
}

/// Checks tool results and attachments for both token shapes and all declared credentials (including short passwords).
pub fn blocks_output(text: &str, credentials: &[Vec<u8>]) -> bool {
    leaks_secret(text, &[]) || contains_credential(text, credentials)
}

/// Conservatively masks whole whitespace-delimited words matching secret shapes before embedding in error strings.
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        let w = word.trim_end();
        let hit = bot_core::redact::has_delimited_secret_prefix(w)
            || !bot_core::redact::secret_spans(w).is_empty();
        if hit {
            out.push_str(MASK);
            out.push_str(&word[w.len()..]);
        } else {
            out.push_str(word);
        }
    }
    out
}

/// Replaces token shapes and profile secrets with `MASK`, falling back to full masking if `leaks_secret` still triggers.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut spans: Vec<Range<usize>> = bot_core::redact::secret_spans(text);
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

    // Assemble token-shaped strings at runtime to avoid triggering secret scanners on source literals.
    fn shaped(prefix: &str, rest: &str) -> String {
        format!("{prefix}-{rest}")
    }

    fn creds(values: &[&str]) -> Vec<Vec<u8>> {
        values.iter().map(|v| v.as_bytes().to_vec()).collect()
    }

    #[test]
    fn blocks_token_shapes_and_real_values() {
        assert!(leaks_secret(
            &format!("token is {} here", shaped("xoxb", "1234567890-abcdefghij")),
            &[]
        ));
        assert!(leaks_secret(
            &format!("key {}", shaped("xapp", "1-A0123456789-abcdef")),
            &[]
        ));
        assert!(leaks_secret(
            &shaped("sk", "3dabcdefghijklmnopqrstuvwxyz"),
            &[]
        ));
        assert!(leaks_secret(
            "value is super-secret-value-123 here",
            &["super-secret-value-123".to_string()]
        ));
    }

    #[test]
    fn blocks_github_token_shapes() {
        let body = "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8";
        for prefix in ["ghp", "gho", "ghu", "ghs", "ghr"] {
            assert!(
                leaks_secret(&format!("token is {prefix}_{body} here"), &[]),
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
        assert!(!leaks_secret("bot tokens start with xoxb- prefix", &[]));
        assert!(!leaks_secret("some keys use the sk- prefix", &[]));
        assert!(!leaks_secret(
            "GitHub tokens start with ghp_ or github_pat_",
            &[]
        ));
        assert!(!leaks_secret("gho_short is not a token", &[]));
        assert!(!leaks_secret("plain answer", &["short".to_string()]));
    }

    #[test]
    fn output_checks_short_declared_passwords_and_ignores_unset_values() {
        let credentials = creds(&["", "pass1234"]);
        assert!(blocks_output("value(pass1234) here", &credentials));
        assert!(blocks_output(&shaped("xoxb", "1234567890"), &[]));
        assert!(!blocks_output("normal value 1 / 1234.5678", &credentials));
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
        let s = scrub(&format!("fail {bot} end key={key} more"));
        assert!(!s.contains(&bot) && !s.contains(&key));
        assert!(s.contains("fail") && s.contains("end"));
        let safe = "region=ASIA-northeast1 EASTASIA ASIA task-123";
        assert_eq!(scrub(safe), safe);
    }

    #[test]
    fn redact_hides_only_the_token_and_keeps_the_sentence() {
        let token = shaped("xoxb", "1234567890-abcdefghij");
        let out = redact(&format!("error: call with token {token} was rejected"), &[]);
        assert_eq!(out, "error: call with token <redacted> was rejected");
        assert!(!leaks_secret(&out, &[]));
    }

    #[test]
    fn redact_hides_every_profile_secret_occurrence() {
        let secrets = ["super-secret-value-123".to_string()];
        let out = redact(
            "value is super-secret-value-123 (again: super-secret-value-123).",
            &secrets,
        );
        assert_eq!(out, "value is <redacted> (again: <redacted>).");
        assert!(!leaks_secret(&out, &secrets));
    }

    #[test]
    fn redact_makes_one_mask_where_a_secret_sits_inside_a_token() {
        let token = shaped("xoxb", "1234567890-abcdefghij");
        let secrets = ["1234567890-abcdefghij".to_string()];
        assert_eq!(redact(&token, &secrets), "<redacted>");
        let two = ["AAAAAAAAAAAA".to_string(), "BBBBBBBBBBBB".to_string()];
        assert_eq!(
            redact("AAAAAAAAAAAABBBBBBBBBBBB end", &two),
            "<redacted> end"
        );
    }

    #[test]
    fn redact_leaves_plain_text_and_short_values_alone() {
        for text in [
            "bot tokens start with xoxb- prefix",
            "some keys use the sk- prefix",
            "GitHub tokens start with ghp_ or github_pat_",
            "plain answer",
        ] {
            assert_eq!(redact(text, &["short".to_string()]), text);
        }
    }

    #[test]
    fn redact_handles_github_shapes_and_text_around_multibyte() {
        let body = "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8";
        let pat = format!("{}_{}_{}", "github_pat", "11AAAAAAA0abcdefghijkl", "rest");
        for text in [
            format!("★★ghp_{body}★★"),
            format!("a {pat} b"),
            format!(
                "{}\n{}",
                shaped("sk", "3dabcdefghijklmnopqrstuvwxyz"),
                "next line"
            ),
        ] {
            let out = redact(&text, &[]);
            assert!(out.contains(MASK), "{text}");
            assert!(!leaks_secret(&out, &[]), "{text}");
        }
        assert_eq!(redact(&format!("★★ghp_{body}★★"), &[]), "★★<redacted>★★");
        assert_eq!(redact(&format!("a {pat} b"), &[]), "a <redacted> b");
    }

    #[test]
    fn blocks_output_and_redact_catch_cloud_stripe_and_jwt_shapes() {
        let aws = ["AKIA", "IOSFODNN7EXAMPLE"].concat();
        let gcp = ["AIza", "SyA_1234567890-abcdef"].concat();
        let stripe = ["sk_live_", "1234567890abcdef"].concat();
        let jwt = format!(
            "{}.{}.{}",
            "eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiJhYmMifQ", "c2lnbmF0dXJlLXZhbHVl"
        );
        for secret in [&aws, &gcp, &stripe, &jwt] {
            let line = format!("key={secret} end");
            assert!(blocks_output(&line, &[]), "{secret}");
            assert_eq!(redact(&line, &[]), "key=<redacted> end", "{secret}");
            assert!(
                !scrub(&format!("err {secret} ok")).contains(secret),
                "{secret}"
            );
        }
    }
}
