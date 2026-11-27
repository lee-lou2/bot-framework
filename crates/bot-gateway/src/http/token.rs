//! HS256 JWT verification for HTTP webhook endpoints.
//!
//! Verifies the constant-time HMAC-SHA256 signature before decoding claims; accepts only `alg = "HS256"` and
//! rejects `crit` headers.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

pub const LEEWAY_SECS: u64 = 30;
pub const MAX_TOKEN_BYTES: usize = 4096;
/// Minimum HS256 key length per RFC 7518.
pub const MIN_KEY_BYTES: usize = 32;

#[derive(Debug, Eq, PartialEq)]
pub enum Reject {
    Malformed,
    Algorithm,
    Signature,
    Issuer,
    Audience,
    Expired,
    NotYet,
    /// Remaining token lifetime exceeds the host-configured maximum.
    TooLong,
    Subject,
}

pub struct Expect<'a> {
    pub issuer: &'a str,
    pub audience: &'a str,
    pub max_age_secs: Option<u64>,
}

fn decode(part: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(part).ok()
}

fn object(bytes: &[u8]) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_slice(bytes).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

fn signed_payload(token: &str, key: &[u8]) -> Result<serde_json::Map<String, Value>, Reject> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(Reject::Malformed);
    }
    let (signing_input, signature) = token.rsplit_once('.').ok_or(Reject::Malformed)?;
    let (header, payload) = signing_input.split_once('.').ok_or(Reject::Malformed)?;
    if payload.contains('.') {
        return Err(Reject::Malformed);
    }
    let header = decode(header)
        .and_then(|bytes| object(&bytes))
        .ok_or(Reject::Malformed)?;
    if header.get("alg").and_then(Value::as_str) != Some("HS256") || header.contains_key("crit") {
        return Err(Reject::Algorithm);
    }
    let signature = decode(signature).ok_or(Reject::Malformed)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| Reject::Signature)?;
    mac.update(signing_input.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| Reject::Signature)?;
    decode(payload)
        .and_then(|bytes| object(&bytes))
        .ok_or(Reject::Malformed)
}

fn seconds(value: Option<&Value>) -> Option<f64> {
    value?.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}

pub fn verify(token: &str, key: &[u8], expect: &Expect, now: u64) -> Result<String, Reject> {
    let claims = signed_payload(token, key)?;
    if claims.get("iss").and_then(Value::as_str) != Some(expect.issuer) {
        return Err(Reject::Issuer);
    }
    let audience_ok = match claims.get("aud") {
        Some(Value::String(one)) => one == expect.audience,
        Some(Value::Array(many)) => many.iter().any(|a| a.as_str() == Some(expect.audience)),
        _ => false,
    };
    if !audience_ok {
        return Err(Reject::Audience);
    }
    let now = now as f64;
    let leeway = LEEWAY_SECS as f64;
    let exp = seconds(claims.get("exp")).ok_or(Reject::Expired)?;
    if now >= exp + leeway {
        return Err(Reject::Expired);
    }
    if let Some(nbf) = claims.get("nbf")
        && seconds(Some(nbf)).is_none_or(|nbf| nbf > now + leeway)
    {
        return Err(Reject::NotYet);
    }
    if let Some(max) = expect.max_age_secs
        && exp > now + max as f64 + leeway
    {
        return Err(Reject::TooLong);
    }
    match claims.get("sub").and_then(Value::as_str) {
        Some(sub) if !sub.is_empty() => Ok(sub.to_owned()),
        _ => Err(Reject::Subject),
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn sign(claims: &Value, key: &[u8]) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("valid HMAC key");
    mac.update(format!("{header}.{payload}").as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{header}.{payload}.{signature}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
    const NOW: u64 = 1_800_000_000;

    fn expect() -> Expect<'static> {
        Expect {
            issuer: "https://issuer.example",
            audience: "bot",
            max_age_secs: None,
        }
    }

    fn claims() -> Value {
        json!({"iss": "https://issuer.example", "aud": "bot", "sub": "svc", "exp": NOW + 60})
    }

    #[test]
    fn accepts_an_independent_reference_signature() {
        // Standard RFC/jwt.io HS256 vector split into segments to avoid secret-scanner false positives.
        let token = [
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ",
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        ]
        .join(".");
        let token = token.as_str();
        let payload = signed_payload(token, b"your-256-bit-secret").unwrap();
        assert_eq!(payload["sub"], "1234567890");
        assert_eq!(
            signed_payload(token, b"another-secret").unwrap_err(),
            Reject::Signature
        );
    }

    #[test]
    fn verifies_a_well_formed_token_and_returns_the_subject() {
        assert_eq!(
            verify(&sign(&claims(), KEY), KEY, &expect(), NOW).unwrap(),
            "svc"
        );
        let mut many = claims();
        many["aud"] = json!(["other", "bot"]);
        assert!(verify(&sign(&many, KEY), KEY, &expect(), NOW).is_ok());
    }

    #[test]
    fn rejects_wrong_key_issuer_audience_and_subject() {
        let token = sign(&claims(), KEY);
        assert_eq!(
            verify(&token, b"x".repeat(32).as_slice(), &expect(), NOW),
            Err(Reject::Signature)
        );
        for (field, value, want) in [
            ("iss", json!("https://evil.example"), Reject::Issuer),
            ("aud", json!("someone-else"), Reject::Audience),
            ("aud", json!(["a", "b"]), Reject::Audience),
            ("sub", json!(""), Reject::Subject),
            ("sub", json!(7), Reject::Subject),
        ] {
            let mut bad = claims();
            bad[field] = value;
            assert_eq!(
                verify(&sign(&bad, KEY), KEY, &expect(), NOW),
                Err(want),
                "{field}"
            );
        }
        let mut no_sub = claims();
        no_sub.as_object_mut().unwrap().remove("sub");
        assert_eq!(
            verify(&sign(&no_sub, KEY), KEY, &expect(), NOW),
            Err(Reject::Subject)
        );
    }

    #[test]
    fn enforces_expiry_with_a_small_leeway_and_not_before() {
        let at = |exp: u64, now: u64| {
            let mut c = claims();
            c["exp"] = json!(exp);
            verify(&sign(&c, KEY), KEY, &expect(), now)
        };
        assert!(at(NOW, NOW).is_ok(), "within clock leeway");
        assert!(at(NOW, NOW + LEEWAY_SECS - 1).is_ok());
        assert_eq!(at(NOW, NOW + LEEWAY_SECS), Err(Reject::Expired));
        let mut missing = claims();
        missing.as_object_mut().unwrap().remove("exp");
        assert_eq!(
            verify(&sign(&missing, KEY), KEY, &expect(), NOW),
            Err(Reject::Expired)
        );
        let mut early = claims();
        early["nbf"] = json!(NOW + 600);
        assert_eq!(
            verify(&sign(&early, KEY), KEY, &expect(), NOW),
            Err(Reject::NotYet)
        );
        early["nbf"] = json!("soon");
        assert_eq!(
            verify(&sign(&early, KEY), KEY, &expect(), NOW),
            Err(Reject::NotYet)
        );
    }

    #[test]
    fn caps_the_remaining_lifetime_when_the_host_asks() {
        let capped = Expect {
            max_age_secs: Some(300),
            ..expect()
        };
        let mut long = claims();
        long["exp"] = json!(NOW + 86_400);
        assert_eq!(
            verify(&sign(&long, KEY), KEY, &capped, NOW),
            Err(Reject::TooLong)
        );
        assert!(verify(&sign(&claims(), KEY), KEY, &capped, NOW).is_ok());
    }

    fn forged(header: &str, claims: &Value, key: &[u8]) -> String {
        let header = URL_SAFE_NO_PAD.encode(header);
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
        mac.update(format!("{header}.{payload}").as_bytes());
        format!(
            "{header}.{payload}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    }

    #[test]
    fn refuses_other_algorithms_critical_headers_and_malformed_tokens() {
        let c = claims();
        assert_eq!(
            verify(&forged(r#"{"alg":"none"}"#, &c, KEY), KEY, &expect(), NOW),
            Err(Reject::Algorithm)
        );
        assert_eq!(
            verify(&forged(r#"{"alg":"HS512"}"#, &c, KEY), KEY, &expect(), NOW),
            Err(Reject::Algorithm)
        );
        assert_eq!(
            verify(&forged(r#"{"alg":"hs256"}"#, &c, KEY), KEY, &expect(), NOW),
            Err(Reject::Algorithm)
        );
        assert_eq!(
            verify(
                &forged(r#"{"alg":"HS256","crit":["exp"]}"#, &c, KEY),
                KEY,
                &expect(),
                NOW
            ),
            Err(Reject::Algorithm)
        );
        let good = sign(&c, KEY);
        for bad in [
            "",
            "a.b",
            "a.b.c.d",
            "..",
            &format!("{good}x"),
            &format!("{good}.extra"),
            &"a".repeat(MAX_TOKEN_BYTES + 1),
        ] {
            assert!(verify(bad, KEY, &expect(), NOW).is_err(), "{bad:?}");
        }
        let unsigned = good.rsplit_once('.').unwrap().0.to_string() + ".";
        assert!(verify(&unsigned, KEY, &expect(), NOW).is_err());
    }
}
