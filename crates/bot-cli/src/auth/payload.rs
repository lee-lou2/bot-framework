//! Host-only encrypted credential payload schemas, allowlisted headers, and token validation.

use bot_broker::mcp::remote::Remote;
use bot_core::auth::CredentialKind;
use serde::{Deserialize, Serialize};

/// Non-secret header names permitted alongside managed OAuth credentials.
pub const ALLOWED_EXTRA_HEADERS: &[&str] = &[
    "chatgpt-account-id",
    "openai-organization",
    "x-goog-user-project",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    #[default]
    Form,
    Json,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "v1_kind", rename_all = "snake_case")]
pub enum CredentialPayload {
    SubscriptionToken {
        provider: String,
        token: String,
    },
    OAuth {
        token_endpoint: String,
        client_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
        access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<(String, String)>,
        #[serde(default)]
        body_format: BodyFormat,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issuer: Option<String>,
    },
    ServiceAccount {
        client_email: String,
        private_key_pem: String,
        token_uri: String,
        scope: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_ms: Option<i64>,
    },
}

impl std::fmt::Debug for CredentialPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubscriptionToken { provider, .. } => {
                write!(
                    f,
                    "CredentialPayload::SubscriptionToken({provider}, [redacted])"
                )
            }
            Self::OAuth { .. } => f.write_str("CredentialPayload::OAuth([redacted])"),
            Self::ServiceAccount { .. } => {
                f.write_str("CredentialPayload::ServiceAccount([redacted])")
            }
        }
    }
}

pub fn is_allowed_extra_header(name: &str) -> bool {
    ALLOWED_EXTRA_HEADERS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

pub fn token_value_ok(value: &str) -> bool {
    !value.is_empty() && value.len() <= 16_384 && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

pub fn extra_header_value_ok(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Validates Claude setup-token shape (`sk-ant-oat...`); rejects refresh/API tokens.
pub fn is_claude_subscription_token(token: &str) -> bool {
    token.starts_with("sk-ant-oat") && token.len() > "sk-ant-oat".len() && token_value_ok(token)
}

/// Detects Claude subscription tokens even if placed into a static secret slot.
pub fn looks_like_subscription_secret(bytes: &[u8]) -> bool {
    let trimmed = bytes.trim_ascii();
    trimmed.starts_with(b"sk-ant-oat")
}

/// Canonicalizes a token endpoint URL into `host:port`, enforcing HTTPS (or loopback HTTP in tests).
pub fn canonical_host_port(url: &str, allow_loopback_http: bool) -> Result<String, String> {
    let parsed = Remote::parse(url, None)?;
    if !parsed.tls && !allow_loopback_http {
        return Err("token endpoint must use https".to_owned());
    }
    Ok(parsed.target_text())
}

impl CredentialPayload {
    pub fn kind(&self) -> CredentialKind {
        match self {
            Self::SubscriptionToken { .. } => CredentialKind::SubscriptionToken,
            Self::OAuth { .. } => CredentialKind::OAuth,
            Self::ServiceAccount { .. } => CredentialKind::ServiceAccount,
        }
    }

    pub fn token_endpoint(&self) -> Option<&str> {
        match self {
            Self::SubscriptionToken { .. } => None,
            Self::OAuth { token_endpoint, .. } => Some(token_endpoint.as_str()),
            Self::ServiceAccount { token_uri, .. } => Some(token_uri.as_str()),
        }
    }

    pub fn expires_ms(&self) -> Option<i64> {
        match self {
            Self::SubscriptionToken { .. } => None,
            Self::OAuth { expires_ms, .. } | Self::ServiceAccount { expires_ms, .. } => *expires_ms,
        }
    }

    pub fn secret_values(&self) -> Vec<&str> {
        match self {
            Self::SubscriptionToken { token, .. } => vec![token.as_str()],
            Self::OAuth {
                client_secret,
                access_token,
                refresh_token,
                ..
            } => {
                let mut out = vec![access_token.as_str()];
                if let Some(rt) = refresh_token.as_deref() {
                    out.push(rt);
                }
                if let Some(cs) = client_secret.as_deref() {
                    out.push(cs);
                }
                out
            }
            Self::ServiceAccount {
                private_key_pem,
                access_token,
                ..
            } => {
                let mut out = vec![private_key_pem.as_str()];
                if let Some(at) = access_token.as_deref() {
                    out.push(at);
                }
                out
            }
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::SubscriptionToken { provider, token } => {
                if provider == "claude" {
                    if !is_claude_subscription_token(token) {
                        return Err("invalid claude setup-token prefix");
                    }
                    Ok(())
                } else {
                    Err("unsupported subscription token provider")
                }
            }
            Self::OAuth {
                client_id,
                access_token,
                refresh_token,
                client_secret,
                headers,
                subscription_provider,
                ..
            } => {
                if client_id.is_empty() || !token_value_ok(access_token) {
                    return Err("invalid oauth token or client_id");
                }
                if let Some(rt) = refresh_token
                    && !token_value_ok(rt)
                {
                    return Err("invalid refresh_token");
                }
                if let Some(cs) = client_secret
                    && !token_value_ok(cs)
                {
                    return Err("invalid client_secret");
                }
                if let Some(sub) = subscription_provider
                    && !matches!(sub.as_str(), "codex")
                {
                    return Err("unsupported oauth subscription provider");
                }
                for (name, value) in headers {
                    if !is_allowed_extra_header(name) || !extra_header_value_ok(value) {
                        return Err("disallowed or invalid extra header");
                    }
                }
                Ok(())
            }
            Self::ServiceAccount {
                client_email,
                private_key_pem,
                scope,
                access_token,
                ..
            } => {
                if client_email.is_empty() || scope.trim().is_empty() || private_key_pem.is_empty()
                {
                    return Err("incomplete service account payload");
                }
                if let Some(at) = access_token
                    && !token_value_ok(at)
                {
                    return Err("invalid cached access_token");
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_claude_setup_token_and_extra_headers() {
        assert!(is_claude_subscription_token("sk-ant-oat01-example"));
        assert!(!is_claude_subscription_token("sk-ant-api03-example"));
        assert!(!is_claude_subscription_token("sk-ant-ort01-example"));
        assert!(!is_claude_subscription_token("sk-ant-oat"));
        assert!(looks_like_subscription_secret(b"sk-ant-oat01-secret\n"));
        assert!(!looks_like_subscription_secret(b"sk-ant-api03-secret"));
        assert!(is_allowed_extra_header("ChatGPT-Account-Id"));
        assert!(is_allowed_extra_header("x-goog-user-project"));
        assert!(!is_allowed_extra_header("authorization"));
        assert!(!is_allowed_extra_header("x-custom"));
    }

    #[test]
    fn canonical_host_port_enforces_https_and_normalizes_port() {
        assert_eq!(
            canonical_host_port("https://AUTH.Example.com/oauth/token", false).unwrap(),
            "auth.example.com:443"
        );
        assert_eq!(
            canonical_host_port("https://auth.example.com:8443/token", false).unwrap(),
            "auth.example.com:8443"
        );
        assert!(canonical_host_port("http://127.0.0.1:8080/token", false).is_err());
        assert_eq!(
            canonical_host_port("http://127.0.0.1:8080/token", true).unwrap(),
            "127.0.0.1:8080"
        );
        assert!(canonical_host_port("http://auth.example.com/token", true).is_err());
    }
}
