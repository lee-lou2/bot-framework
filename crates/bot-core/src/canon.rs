//! RFC 8785 (JSON Canonicalization Scheme) canonicalization and approval-binding SHA-256.

pub use crate::json::parse;
use serde_json::{Number, Value};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;

/// Escapes only `"`, `\`, and U+0000..=U+001F (leaving `/`, DEL, and non-ASCII verbatim), shared by JCS and approval rendering.
pub fn json_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Exclusive IEEE-754 safe integer bound (2^53); integers at or above this lose exactness and are rejected to prevent approval ambiguity.
const LIMIT: f64 = 9_007_199_254_740_992.0;

/// Returns `None` if any number falls outside the exact safe range or is non-finite.
pub fn canonicalize(value: &Value) -> Option<String> {
    let mut out = String::new();
    write(value, &mut out)?;
    Some(out)
}

pub fn sha256(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

fn write(value: &Value, out: &mut String) -> Option<()> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&number(n)?),
        Value::String(s) => out.push_str(&json_quote(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| cmp_utf16(a.0, b.0));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&json_quote(key));
                out.push(':');
                write(item, out)?;
            }
            out.push('}');
        }
    }
    Some(())
}

/// RFC 8785 requires UTF-16 code unit ordering, not UTF-8 byte or Unicode scalar order.
pub fn cmp_utf16(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn number(n: &Number) -> Option<String> {
    if let Some(i) = n.as_i64() {
        return ((i as f64).abs() < LIMIT).then(|| i.to_string());
    }
    if let Some(u) = n.as_u64() {
        return ((u as f64) < LIMIT).then(|| u.to_string());
    }
    let x = n.as_f64()?;
    if !x.is_finite() || (x.fract() == 0.0 && x.abs() >= LIMIT) {
        return None;
    }
    Some(es_number(x))
}

/// ECMAScript `Number::toString` (RFC 8785 §3.2.2.3) for finite numbers.
pub fn es_number(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let (digits, exp) = shortest(x.abs());
    let k = digits.len() as i32;
    let n = exp + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        if k == 1 {
            format!("{digits}e{sign}{}", e.abs())
        } else {
            format!("{}.{}e{sign}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    if x < 0.0 { format!("-{body}") } else { body }
}

/// Shortest round-trip decimal digits and base-10 exponent.
/// Rust breaks exact midpoint ties away from zero, whereas ECMAScript breaks ties to an even last digit.
fn shortest(x: f64) -> (String, i32) {
    let sci = format!("{x:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    // Midpoint ties only occur at 15+ significant digits where formatting fills the rounding interval.
    if digits.len() >= 15
        && let Some(even) = tie_even(x, &digits, exp)
    {
        return (even, exp);
    }
    (digits, exp)
}

/// When `x` lies at an exact midpoint between two `digits`-length decimals, returns the even-ending candidate that round-trips to `x`.
fn tie_even(x: f64, digits: &str, exp: i32) -> Option<String> {
    let k = digits.len();
    let exact = format!("{x:.800e}"); // Exact decimal expansion (f64 terminates within 767 digits).
    let (mantissa, _) = exact.split_once('e')?;
    let all: String = mantissa.chars().filter(|c| *c != '.').collect();
    let expansion = all.trim_end_matches('0');
    if expansion.len() != k + 1 || !expansion.ends_with('5') {
        return None;
    }
    let lower = expansion[..k].to_owned();
    let upper = increment(&lower).filter(|up| up.len() == k)?;
    [lower, upper].into_iter().find(|candidate| {
        let last = candidate.as_bytes()[k - 1] - b'0';
        let text = format!("{}.{}e{exp}", &candidate[..1], &candidate[1..]);
        last.is_multiple_of(2) && text.parse::<f64>().is_ok_and(|back| back == x)
    })
}

/// Increments decimal digits by 1; carry overflow returns `None` (filtered by caller).
fn increment(digits: &str) -> Option<String> {
    let mut bytes = digits.as_bytes().to_vec();
    for byte in bytes.iter_mut().rev() {
        if *byte == b'9' {
            *byte = b'0';
        } else {
            *byte += 1;
            return String::from_utf8(bytes).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference values from Node `String(Number)`, including RFC 8785 Appendix B samples.
    const NUMBERS: [(u64, &str); 37] = [
        (0x0000000000000000, "0"),
        (0x8000000000000000, "0"),
        (0x0000000000000001, "5e-324"),
        (0x8000000000000001, "-5e-324"),
        (0x7fefffffffffffff, "1.7976931348623157e+308"),
        (0xffefffffffffffff, "-1.7976931348623157e+308"),
        (0x4340000000000000, "9007199254740992"),
        (0xc340000000000000, "-9007199254740992"),
        (0x4430000000000000, "295147905179352830000"),
        (0x44b52d02c7e14af5, "9.999999999999997e+22"),
        (0x44b52d02c7e14af6, "1e+23"),
        (0x44b52d02c7e14af7, "1.0000000000000001e+23"),
        (0x444b1ae4d6e2ef4e, "999999999999999700000"),
        (0x444b1ae4d6e2ef4f, "999999999999999900000"),
        (0x444b1ae4d6e2ef50, "1e+21"),
        (0x3eb0c6f7a0b5ed8c, "9.999999999999997e-7"),
        (0x3eb0c6f7a0b5ed8d, "0.000001"),
        (0x41b3de4355555553, "333333333.3333332"),
        (0x41b3de4355555554, "333333333.33333325"),
        (0x41b3de4355555555, "333333333.3333333"),
        (0x41b3de4355555556, "333333333.3333334"),
        (0x41b3de4355555557, "333333333.33333343"),
        (0xbecbf647612f3696, "-0.0000033333333333333333"),
        (0x43143ff3c1cb0959, "1424953923781206.2"),
        (0x3ff0000000000000, "1"),
        (0x4059000000000000, "100"),
        (0x3fb999999999999a, "0.1"),
        (0xc024000000000000, "-10"),
        (0x3e45798ee2308c3a, "1e-8"),
        (0x4415af1d78b58c40, "100000000000000000000"),
        (0x44b52d02c7e14af6, "1e+23"),
        // Midpoint tie-to-even samples (Node reference)
        (0x43294c4bc3f430fd, "3560381355792510.5"),
        (0xc316eb95be23d629, "-1612869465077130.2"),
        (0xc3137215c50c744a, "-1368365595827474.5"),
        (0xc32eba681dab06b9, "-4324602819216220.5"),
        (0xc32907877f261043, "-3522576476473377.5"),
        (0x4314d39d02d22b65, "1465542711216857.2"),
    ];

    #[test]
    fn numbers_match_ecmascript_for_the_rfc_8785_samples() {
        for (bits, expected) in NUMBERS {
            assert_eq!(es_number(f64::from_bits(bits)), expected, "{bits:#x}");
        }
    }

    #[test]
    fn keys_sort_by_utf16_code_units_not_code_points() {
        let doc = parse(
            "{\"\\u20ac\":\"Euro Sign\",\"\\r\":\"Carriage Return\",\"\\ufb33\":\"Hebrew Letter Dalet With Dagesh\",\
             \"1\":\"One\",\"\\ud83d\\ude00\":\"Emoji: Grinning Face\",\"\\u0080\":\"Control\",\
             \"\\u00f6\":\"Latin Small Letter O With Diaeresis\"}",
        )
        .unwrap();
        // U+1F600 (surrogate pair starting with D83D) sorts before U+FB33 in UTF-16, unlike scalar order.
        let expected = "{\"\\r\":\"Carriage Return\",\"1\":\"One\",\"\u{80}\":\"Control\",\
            \"\u{f6}\":\"Latin Small Letter O With Diaeresis\",\"\u{20ac}\":\"Euro Sign\",\
            \"\u{1f600}\":\"Emoji: Grinning Face\",\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}";
        assert_eq!(canonicalize(&doc).unwrap(), expected);
    }

    #[test]
    fn the_rfc_8785_sample_document_canonicalizes_as_published_except_unsafe_numbers() {
        // Omits 1E30 from the RFC sample: integers >= 2^53 are rejected to avoid approval ambiguity.
        let doc = parse(
            r##"{"numbers":[333333333.33333329,4.50,2e-3,0.000000000000000000000000001],
                "string":"\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/","literals":[null,true,false]}"##,
        )
        .unwrap();
        let expected = r##"{"literals":[null,true,false],"numbers":[333333333.3333333,4.5,0.002,1e-27],"string":"€$\u000f\nA'B\"\\\\\"/"}"##;
        assert_eq!(canonicalize(&doc).unwrap(), expected);
    }

    #[test]
    fn spelling_and_key_order_do_not_change_the_canonical_form_or_hash() {
        let a = parse(r#"{"b":[1.0,2e0],"a":"x","c":{"y":1,"x":2}}"#).unwrap();
        let b = parse(r#"{ "c": {"x":2,"y":1.00}, "a":"x", "b":[1,2] }"#).unwrap();
        let (ca, cb) = (canonicalize(&a).unwrap(), canonicalize(&b).unwrap());
        assert_eq!(ca, r#"{"a":"x","b":[1,2],"c":{"x":2,"y":1}}"#);
        assert_eq!(ca, cb);
        assert_eq!(sha256(&ca), sha256(&cb));
        let c = parse(r#"{"b":[1,2],"a":"x","c":{"y":1,"x":3}}"#).unwrap();
        assert_ne!(sha256(&canonicalize(&c).unwrap()), sha256(&ca));
    }

    #[test]
    fn the_hash_is_sha256_of_the_canonical_bytes() {
        let hex = |bytes: [u8; 32]| format!("{:x}", sha2::digest::Output::<Sha256>::from(bytes));
        assert_eq!(
            hex(sha256("")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(sha256("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn numbers_outside_the_safe_range_are_refused_not_rounded() {
        for bad in [
            "9007199254740992",
            "-9007199254740992",
            "9007199254740993",
            "18446744073709551615",
            "9007199254740993.0",
            "1e300",
            "-1e22",
        ] {
            let doc = parse(&format!("{{\"id\":{bad}}}")).unwrap();
            assert!(canonicalize(&doc).is_none(), "{bad}");
        }
        for ok in [
            "9007199254740991",
            "-9007199254740991",
            "0.5",
            "1e-300",
            "123456789.125",
        ] {
            let doc = parse(&format!("{{\"n\":{ok}}}")).unwrap();
            assert!(canonicalize(&doc).is_some(), "{ok}");
        }
    }

    #[test]
    fn control_characters_and_quotes_are_escaped_the_jcs_way() {
        let doc = parse(r#"{"k\u0001":"a\u007f\u2028\"\\/\b\f"}"#).unwrap();
        assert_eq!(
            canonicalize(&doc).unwrap(),
            "{\"k\\u0001\":\"a\u{7f}\u{2028}\\\"\\\\/\\b\\f\"}"
        );
    }
}
