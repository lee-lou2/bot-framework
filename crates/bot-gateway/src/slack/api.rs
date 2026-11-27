//! Slack Web API HTTP client with single-attempt delivery classification and no hidden retries.
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

const DEFAULT_BASE: &str = "https://slack.com/api/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RESPONSE: usize = 1 << 20;
const DEFAULT_RETRY: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Raw {
    Ok(Value),
    /// Confirmed refusal (`ok: false`) with error code restricted to `[a-z0-9_]`.
    Error(String),
    /// Connect failure before any request bytes were written.
    BeforeWrite,
    RateLimited(Duration),
    /// Pre-execution auth rejection (`invalid_auth`, `token_revoked`, `token_expired`); transient during rotation.
    TokenRejected,
    /// Timeout, 5xx, or unreadable body — execution outcome is unknown.
    Unknown,
}

pub enum Body<'a> {
    Json(&'a Value),
    Form(Vec<(&'static str, String)>),
}

#[derive(Clone)]
pub struct SlackApi {
    http: Client,
    base: String,
}

fn sanitize(code: &str) -> String {
    let clean = !code.is_empty()
        && code.len() <= 40
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if clean {
        code.to_owned()
    } else {
        "unknown_error".into()
    }
}

/// Only 2xx JSON responses with a boolean `ok` count as Slack responses; 5xx proxy bodies stay `Unknown`.
pub fn classify(status: u16, retry_after: Option<&str>, body: &[u8]) -> Raw {
    let wait = || {
        retry_after
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(DEFAULT_RETRY, Duration::from_secs)
            .max(Duration::from_secs(1))
    };
    if status == 429 {
        return Raw::RateLimited(wait());
    }
    if !(200..300).contains(&status) || body.len() > MAX_RESPONSE {
        return Raw::Unknown;
    }
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Raw::Unknown;
    };
    match value["ok"].as_bool() {
        Some(true) => Raw::Ok(value),
        Some(false) => match value["error"].as_str().filter(|code| !code.is_empty()) {
            Some("ratelimited") => Raw::RateLimited(wait()),
            Some("invalid_auth" | "token_revoked" | "token_expired") => Raw::TokenRejected,
            Some(code) => Raw::Error(sanitize(code)),
            None => Raw::Unknown,
        },
        None => Raw::Unknown,
    }
}

impl SlackApi {
    pub fn new(base: Option<&str>) -> Self {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            // Disable connection pooling and transport-level retries so writes never silently retransmit.
            .pool_max_idle_per_host(0)
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to initialize HTTP client (check CA certificates)");
        let mut base = base.unwrap_or(DEFAULT_BASE).to_owned();
        if !base.ends_with('/') {
            base.push('/');
        }
        Self { http, base }
    }

    pub async fn call(&self, method: &str, token: &str, body: Body<'_>) -> Raw {
        let request = self
            .http
            .post(format!("{}{method}", self.base))
            .bearer_auth(token);
        let request = match body {
            Body::Json(value) => request.json(value),
            Body::Form(form) => request.form(&form),
        };
        self.finish(request.send().await).await
    }

    /// Uploads raw bytes to Slack's pre-signed URL without attaching a Bearer token or logging the URL.
    pub async fn put_bytes(&self, url: &str, bytes: Vec<u8>) -> Raw {
        let sent = self
            .http
            .post(url)
            .timeout(UPLOAD_TIMEOUT)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await;
        match sent {
            Ok(response) if response.status().is_success() => Raw::Ok(Value::Null),
            Ok(response) if response.status().as_u16() == 429 => Raw::RateLimited(DEFAULT_RETRY),
            Ok(_) => Raw::Unknown,
            Err(error) if error.is_connect() => Raw::BeforeWrite,
            Err(_) => Raw::Unknown,
        }
    }

    async fn finish(&self, sent: Result<reqwest::Response, reqwest::Error>) -> Raw {
        let response = match sent {
            Ok(response) => response,
            Err(error) if error.is_connect() => return Raw::BeforeWrite,
            Err(_) => return Raw::Unknown,
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        match response.bytes().await {
            Ok(body) => classify(status, retry_after.as_deref(), &body),
            Err(_) if status == 429 => classify(status, retry_after.as_deref(), b""),
            Err(_) => Raw::Unknown,
        }
    }

    pub async fn open_socket(&self, app_token: &str) -> Option<String> {
        match self
            .call("apps.connections.open", app_token, Body::Form(vec![]))
            .await
        {
            Raw::Ok(value) => value["url"].as_str().map(str::to_owned),
            _ => None,
        }
    }

    pub async fn auth_test(&self, bot_token: &str) -> Option<AuthInfo> {
        match self.call("auth.test", bot_token, Body::Form(vec![])).await {
            Raw::Ok(value) => Some(AuthInfo {
                user_id: value["user_id"].as_str()?.to_owned(),
                team_id: value["team_id"].as_str()?.to_owned(),
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthInfo {
    pub user_id: String,
    pub team_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(status: u16, retry: Option<&str>, body: &str) -> Raw {
        classify(status, retry, body.as_bytes())
    }

    #[test]
    fn only_a_2xx_ok_json_is_confirmed() {
        assert!(matches!(
            raw(200, None, r#"{"ok":true,"ts":"1.1"}"#),
            Raw::Ok(_)
        ));
        assert_eq!(
            raw(200, None, r#"{"ok":false,"error":"channel_not_found"}"#),
            Raw::Error("channel_not_found".into())
        );
        assert_eq!(raw(502, None, r#"{"ok":true}"#), Raw::Unknown);
        assert_eq!(raw(500, None, ""), Raw::Unknown);
        assert_eq!(raw(302, None, ""), Raw::Unknown);
        for body in [
            "",
            "not json",
            r#"{"ts":"1.1"}"#,
            r#"{"ok":false}"#,
            r#"{"ok":"yes"}"#,
        ] {
            assert_eq!(raw(200, None, body), Raw::Unknown, "{body}");
        }
        assert_eq!(
            classify(200, None, &vec![b' '; MAX_RESPONSE + 1]),
            Raw::Unknown
        );
    }

    #[test]
    fn rate_limits_carry_the_wait_and_never_less_than_a_second() {
        assert_eq!(
            raw(429, Some("7"), ""),
            Raw::RateLimited(Duration::from_secs(7))
        );
        assert_eq!(
            raw(429, Some("0"), ""),
            Raw::RateLimited(Duration::from_secs(1))
        );
        assert_eq!(raw(429, None, ""), Raw::RateLimited(DEFAULT_RETRY));
        assert_eq!(raw(429, Some("soon"), ""), Raw::RateLimited(DEFAULT_RETRY));
        assert_eq!(
            raw(200, Some("3"), r#"{"ok":false,"error":"ratelimited"}"#),
            Raw::RateLimited(Duration::from_secs(3))
        );
    }

    #[test]
    fn a_rejected_bot_token_is_not_an_ordinary_refusal() {
        for code in ["invalid_auth", "token_revoked", "token_expired"] {
            let body = format!(r#"{{"ok":false,"error":"{code}"}}"#);
            assert_eq!(raw(200, None, &body), Raw::TokenRejected, "{code}");
        }
        for code in [
            "not_authed",
            "account_inactive",
            "missing_scope",
            "channel_not_found",
            "invalid_blocks",
        ] {
            let body = format!(r#"{{"ok":false,"error":"{code}"}}"#);
            assert_eq!(raw(200, None, &body), Raw::Error(code.into()), "{code}");
        }
        for status in [401, 403, 500, 502] {
            assert_eq!(
                raw(status, None, r#"{"ok":false,"error":"invalid_auth"}"#),
                Raw::Unknown,
                "{status}"
            );
        }
    }

    #[test]
    fn error_codes_are_reduced_to_a_safe_vocabulary() {
        for odd in ["Has Space", "UPPER", &"x".repeat(41), "a-b"] {
            let body = format!(r#"{{"ok":false,"error":"{odd}"}}"#);
            assert_eq!(
                raw(200, None, &body),
                Raw::Error("unknown_error".into()),
                "{odd}"
            );
        }
    }
}
