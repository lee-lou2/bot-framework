use super::*;
use axum::http::HeaderValue;

fn headers(values: &[&str]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for value in values {
        map.append(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
    }
    map
}

const KEY: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn exactly_one_standard_bearer_header_is_read() {
    assert_eq!(presented(&headers(&[&format!("Bearer {KEY}")])), Some(KEY));
    assert_eq!(presented(&headers(&[&format!("bearer {KEY}")])), Some(KEY));
    assert_eq!(presented(&headers(&[&format!("BEARER {KEY}")])), Some(KEY));
    // 헤더가 없거나 둘 이상이면 어느 것도 믿지 않는다(같은 값이어도)
    assert_eq!(presented(&HeaderMap::new()), None);
    let one = format!("Bearer {KEY}");
    assert_eq!(presented(&headers(&[&one, &one])), None);
    assert_eq!(
        presented(&headers(&[
            &one,
            "Bearer other-other-other-other-other-other"
        ])),
        None
    );
}

#[test]
fn other_schemes_and_malformed_values_are_not_keys() {
    for bad in [
        format!("Basic {KEY}"),
        format!("Token {KEY}"),
        KEY.to_owned(),
        format!("Bearer  {KEY}"),
        format!("Bearer {KEY} extra"),
        "Bearer ".to_owned(),
        "Bearer".to_owned(),
    ] {
        assert_eq!(presented(&headers(&[&bad])), None, "{bad:?}");
    }
}

#[test]
fn a_key_is_ascii_without_spaces_and_between_32_and_256_bytes() {
    let ok = |n: usize| presented(&headers(&[&format!("Bearer {}", "k".repeat(n))])).is_some();
    assert!(!ok(31) && ok(32) && ok(256) && !ok(257));
    // 비ASCII 바이트·제어 문자는 헤더 값으로 들어와도 키가 아니다
    let mut map = HeaderMap::new();
    map.append(
        header::AUTHORIZATION,
        HeaderValue::from_bytes(format!("Bearer {}\u{e9}", "k".repeat(40)).as_bytes()).unwrap(),
    );
    assert_eq!(presented(&map), None);
}

#[test]
fn hash_comparison_distinguishes_every_byte() {
    let a: Hash = Sha256::digest(b"a").into();
    assert!(same(&a, &a));
    for at in 0..32 {
        let mut b = a;
        b[at] ^= 1;
        assert!(!same(&a, &b), "{at}");
    }
}
