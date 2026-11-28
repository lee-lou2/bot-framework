//! Google service-account PKCS#8 PEM validation, RS256 JWT-bearer signing, and access-token exchange.

use super::http::AuthHttp;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use bot_broker::resolve::HostPort;
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde_json::{Value, json};

pub const DEFAULT_GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const MIN_EXPIRES_SECS: i64 = 60;
const MAX_EXPIRES_SECS: i64 = 86_400;

#[derive(Clone, Debug)]
pub struct ServiceAccountKey {
    pub client_email: String,
    pub private_key_pem: String,
    pub token_uri: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExchangeError {
    InvalidGrant,
    Transient,
    InvalidConfig,
}

const PEM_DASHES: &str = "-----";

fn pem_fence(edge: &str, label: &str) -> String {
    format!("{PEM_DASHES}{edge} {label}{PEM_DASHES}")
}

/// Strictly validates an unencrypted PKCS#8 private key PEM and returns the parsed `RsaKeyPair`.
pub fn parse_pkcs8_rsa(pem: &str) -> Result<RsaKeyPair, String> {
    if pem.contains(&pem_fence("BEGIN", "ENCRYPTED PRIVATE KEY")) {
        return Err(
            "encrypted private keys are not supported; provide an unencrypted PKCS#8 key"
                .to_owned(),
        );
    }
    if pem.contains(&pem_fence("BEGIN", "RSA PRIVATE KEY")) {
        return Err(
            "PKCS#1 RSA PRIVATE KEY is not supported; provide a PKCS#8 `BEGIN PRIVATE KEY` block"
                .to_owned(),
        );
    }
    let begin = pem_fence("BEGIN", "PRIVATE KEY");
    let end = pem_fence("END", "PRIVATE KEY");
    let Some(after_begin) = pem.split_once(&begin).map(|(_, rest)| rest) else {
        return Err("private_key must contain a PKCS#8 `BEGIN PRIVATE KEY` PEM block".to_owned());
    };
    let Some((body, trailing)) = after_begin.split_once(&end) else {
        return Err("private_key is missing `END PRIVATE KEY` PEM footer".to_owned());
    };
    if trailing.contains(&format!("{PEM_DASHES}BEGIN ")) {
        return Err("private_key must contain exactly one PEM block".to_owned());
    }
    let clean: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if clean.is_empty() {
        return Err("private_key PEM body is empty".to_owned());
    }
    let der = STANDARD
        .decode(clean.as_bytes())
        .map_err(|_| "private_key contains invalid base64".to_owned())?;
    RsaKeyPair::from_pkcs8(&der).map_err(|_| "private_key is not a valid PKCS#8 RSA key".to_owned())
}

/// Parses and validates a Google service-account JSON file without echoing private key bytes.
pub fn parse_service_account_json(
    raw: &str,
    allow_loopback_http: bool,
) -> Result<ServiceAccountKey, String> {
    let doc: Value = serde_json::from_str(raw)
        .map_err(|_| "service account file is not valid JSON".to_owned())?;
    let Some(map) = doc.as_object() else {
        return Err("service account file must be a JSON object".to_owned());
    };
    if map.get("type").and_then(Value::as_str) != Some("service_account") {
        return Err("service account JSON must have `\"type\": \"service_account\"`".to_owned());
    }
    let client_email = map
        .get("client_email")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.contains('@'))
        .ok_or("service account JSON is missing valid `client_email`")?
        .to_owned();
    let private_key_pem = map
        .get("private_key")
        .and_then(Value::as_str)
        .ok_or("service account JSON is missing `private_key`")?
        .to_owned();
    let token_uri = map
        .get("token_uri")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_GOOGLE_TOKEN_URI)
        .to_owned();
    super::payload::canonical_host_port(&token_uri, allow_loopback_http)
        .map_err(|why| format!("service account `token_uri` invalid: {why}"))?;
    parse_pkcs8_rsa(&private_key_pem)?;
    Ok(ServiceAccountKey {
        client_email,
        private_key_pem,
        token_uri,
    })
}

/// Signs an RS256 OAuth2 JWT-bearer assertion for a service account.
pub fn mint_jwt_assertion(
    client_email: &str,
    private_key_pem: &str,
    token_uri: &str,
    scope: &str,
    now_secs: i64,
) -> Result<String, String> {
    let key = parse_pkcs8_rsa(private_key_pem)?;
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = json!({
        "iss": client_email,
        "sub": client_email,
        "scope": scope,
        "aud": token_uri,
        "iat": now_secs,
        "exp": now_secs + 3600,
    });
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
    let signing_input = format!("{header}.{payload}");
    let rng = SystemRandom::new();
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(&RSA_PKCS1_SHA256, &rng, signing_input.as_bytes(), &mut sig)
        .map_err(|_| "failed to sign service-account JWT assertion".to_owned())?;
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(&sig)))
}

/// Exchanges a signed JWT-bearer assertion for a short-lived access token.
#[allow(clippy::too_many_arguments)]
pub async fn exchange_jwt(
    http: &AuthHttp,
    client_email: &str,
    private_key_pem: &str,
    token_uri: &str,
    scope: &str,
    now_ms: i64,
    profile_egress: Option<&[HostPort]>,
    register_secret: &(dyn Fn(&str) + Send + Sync),
) -> Result<(String, i64), ExchangeError> {
    let assertion = mint_jwt_assertion(
        client_email,
        private_key_pem,
        token_uri,
        scope,
        now_ms / 1000,
    )
    .map_err(|_| ExchangeError::InvalidConfig)?;
    register_secret(&assertion);
    let (status, body) = http
        .post_form(
            token_uri,
            &[("grant_type", JWT_BEARER_GRANT), ("assertion", &assertion)],
            profile_egress,
        )
        .await
        .map_err(|_| ExchangeError::Transient)?;
    if (200..300).contains(&status) {
        let Some(token) = body
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|t| super::payload::token_value_ok(t))
        else {
            return Err(ExchangeError::Transient);
        };
        register_secret(token);
        let expires_in = body
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(3600)
            .clamp(MIN_EXPIRES_SECS, MAX_EXPIRES_SECS);
        return Ok((token.to_owned(), now_ms.saturating_add(expires_in * 1000)));
    }
    let error_code = body.get("error").and_then(Value::as_str).unwrap_or("");
    if error_code == "invalid_grant" {
        Err(ExchangeError::InvalidGrant)
    } else if matches!(
        error_code,
        "invalid_client" | "unauthorized_client" | "invalid_scope"
    ) {
        Err(ExchangeError::InvalidConfig)
    } else {
        Err(ExchangeError::Transient)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_pkcs1_encrypted_and_malformed_pems_without_echoing_bytes() {
        let wrap = |label: &str, body: &str| {
            format!(
                "{}\n{body}\n{}",
                pem_fence("BEGIN", label),
                pem_fence("END", label)
            )
        };
        let pkcs1 = wrap("RSA PRIVATE KEY", "SENSITIVE_MARKER_1");
        let err = parse_pkcs8_rsa(&pkcs1).unwrap_err();
        assert!(
            err.contains("PKCS#1") && !err.contains("SENSITIVE_MARKER_1"),
            "{err}"
        );

        let encrypted = wrap("ENCRYPTED PRIVATE KEY", "SENSITIVE_MARKER_2");
        let err = parse_pkcs8_rsa(&encrypted).unwrap_err();
        assert!(
            err.contains("encrypted") && !err.contains("SENSITIVE_MARKER_2"),
            "{err}"
        );

        let bad_b64 = wrap("PRIVATE KEY", "!!!SENSITIVE_MARKER_3!!!");
        let err = parse_pkcs8_rsa(&bad_b64).unwrap_err();
        assert!(!err.contains("SENSITIVE_MARKER_3"), "{err}");

        let bad_der = wrap("PRIVATE KEY", "AAAA");
        assert!(parse_pkcs8_rsa(&bad_der).is_err());
    }

    #[test]
    fn mints_verifiable_rs256_jwt_from_generated_pkcs8_key() {
        let pem = generate_test_pkcs8_pem();
        let jwt = mint_jwt_assertion(
            "bot@proj.iam.gserviceaccount.com",
            &pem,
            DEFAULT_GOOGLE_TOKEN_URI,
            "https://www.googleapis.com/auth/cloud-platform",
            1_700_000_000,
        )
        .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "bot@proj.iam.gserviceaccount.com");
        assert_eq!(claims["aud"], DEFAULT_GOOGLE_TOKEN_URI);
        assert_eq!(claims["exp"], 1_700_003_600);
        // Verify signature using ring's RSA_PKCS1_2048_8192_SHA256 verifier.
        let key = parse_pkcs8_rsa(&pem).unwrap();
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            key.public().as_ref(),
        );
        let signed = format!("{}.{}", parts[0], parts[1]);
        let sig = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        public.verify(signed.as_bytes(), &sig).unwrap();
    }

    pub(crate) fn generate_test_pkcs8_pem() -> String {
        let out = std::process::Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
            ])
            .output()
            .expect("openssl genpkey");
        assert!(out.status.success(), "openssl genpkey failed");
        String::from_utf8(out.stdout).unwrap()
    }
}
