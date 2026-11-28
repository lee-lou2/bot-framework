//! Telegram Bot API HTTPS client with single-attempt delivery classification and zero token leakage.
//!
//! Bot tokens appear in Telegram's URL path (`/bot<token>/<method>`), so this client never logs
//! URLs, raw `reqwest::Error` strings, or unredacted response bodies.
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

const DEFAULT_BASE: &str = "https://api.telegram.org/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_OVERHEAD: Duration = Duration::from_secs(15);
const MAX_RESPONSE: usize = 4 << 20;
const DEFAULT_RETRY: Duration = Duration::from_secs(30);
const MAX_ERROR_CHARS: usize = 80;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Raw {
    Ok(Value),
    /// Confirmed HTTP 409 Conflict (concurrent `getUpdates` consumer or active webhook).
    Conflict,
    /// Confirmed refusal (`ok: false`) with sanitized ASCII description (never echoing tokens).
    Error(u16, String),
    /// Connect failure before any request bytes were written.
    BeforeWrite,
    RateLimited(Duration),
    /// Pre-execution auth rejection (HTTP 401 Unauthorized); transient during token rotation.
    TokenRejected,
    /// Timeout, 5xx, or unreadable body — execution outcome is unknown.
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BotInfo {
    pub id: u64,
    pub username: Option<String>,
}

#[derive(Clone)]
pub struct TelegramApi {
    http: Client,
    base: String,
}

fn sanitize_desc(raw: &str, token: &str) -> String {
    let scrubbed = if token.is_empty() {
        raw.to_owned()
    } else {
        raw.replace(token, "[redacted]")
    };
    let clean: String = scrubbed
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_ERROR_CHARS)
        .collect();
    if clean.trim().is_empty() {
        "telegram_error".into()
    } else {
        clean
    }
}

fn parse_retry_after(header: Option<&str>, body: Option<&Value>) -> Duration {
    let from_body = body
        .and_then(|v| v["parameters"]["retry_after"].as_u64())
        .map(Duration::from_secs);
    let from_header = header
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    from_body
        .or(from_header)
        .unwrap_or(DEFAULT_RETRY)
        .max(Duration::from_secs(1))
}

/// Classifies a Telegram Bot API HTTP response without leaking token material.
pub fn classify(status: u16, retry_after: Option<&str>, body: &[u8], token: &str) -> Raw {
    if status == 409 {
        return Raw::Conflict;
    }
    if status == 401 {
        return Raw::TokenRejected;
    }
    if body.len() > MAX_RESPONSE {
        return if status == 429 {
            Raw::RateLimited(parse_retry_after(retry_after, None))
        } else {
            Raw::Unknown
        };
    }
    let parsed = serde_json::from_slice::<Value>(body).ok();
    if status == 429 {
        return Raw::RateLimited(parse_retry_after(retry_after, parsed.as_ref()));
    }
    if status >= 500 || (300..400).contains(&status) {
        return Raw::Unknown;
    }
    let Some(value) = parsed else {
        return Raw::Unknown;
    };
    match value["ok"].as_bool() {
        Some(true) if (200..300).contains(&status) => Raw::Ok(value["result"].clone()),
        Some(false) => {
            let code = value["error_code"]
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .unwrap_or(status);
            if code == 409 {
                return Raw::Conflict;
            }
            if code == 401 {
                return Raw::TokenRejected;
            }
            if code == 429 {
                return Raw::RateLimited(parse_retry_after(retry_after, Some(&value)));
            }
            if code >= 500 {
                return Raw::Unknown;
            }
            let desc = value["description"].as_str().unwrap_or("telegram_error");
            Raw::Error(code, sanitize_desc(desc, token))
        }
        _ => Raw::Unknown,
    }
}

fn sanitize_filename(name: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '"' | '\\' | '/' | '\r' | '\n'))
        .take(200)
        .collect();
    if clean.is_empty() {
        "attachment.bin".into()
    } else {
        clean
    }
}

/// File payload for Telegram multipart media uploads (`sendDocument` / `sendPhoto`).
#[derive(Clone, Copy, Debug)]
pub struct MediaFile<'a> {
    pub name: &'a str,
    pub mime: &'a str,
    pub bytes: &'a [u8],
}

/// Optional routing fields for Telegram multipart media uploads (`sendDocument` / `sendPhoto`).
#[derive(Clone, Copy, Debug, Default)]
pub struct MediaOptions<'a> {
    pub caption: Option<&'a str>,
    pub thread_id: Option<u64>,
    pub reply_to_message_id: Option<u64>,
}

/// Builds a `multipart/form-data` payload for `sendDocument` or `sendPhoto` without extra crate dependencies.
pub(crate) fn build_media_multipart(
    field_name: &str,
    chat_id: &str,
    file: MediaFile<'_>,
    options: MediaOptions<'_>,
) -> (String, Vec<u8>) {
    let reply_params = options.reply_to_message_id.map(|message_id| {
        serde_json::json!({
            "message_id": message_id,
            "allow_sending_without_reply": true,
        })
        .to_string()
    });
    // Choose a boundary guaranteed not to appear inside the payload bytes.
    let mut suffix = 0u64;
    let boundary = loop {
        let candidate = format!("----botfw-tg-boundary-{suffix:016x}");
        let needle = candidate.as_bytes();
        let clashes = file.bytes.windows(needle.len()).any(|w| w == needle)
            || options.caption.is_some_and(|c| c.contains(&candidate))
            || reply_params
                .as_ref()
                .is_some_and(|r| r.contains(&candidate));
        if !clashes {
            break candidate;
        }
        suffix = suffix.wrapping_add(1);
    };
    let mut out = Vec::with_capacity(file.bytes.len() + 640);
    let push_field = |out: &mut Vec<u8>, name: &str, value: &str| {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    };
    push_field(&mut out, "chat_id", chat_id);
    if let Some(thread_id) = options.thread_id {
        push_field(&mut out, "message_thread_id", &thread_id.to_string());
    }
    if let Some(reply_params) = reply_params.as_deref() {
        push_field(&mut out, "reply_parameters", reply_params);
    }
    if let Some(caption) = options.caption {
        push_field(&mut out, "caption", caption);
    }
    let safe_name = sanitize_filename(file.name);
    let safe_mime = if file
        .mime
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'+' | b'-'))
        && !file.mime.is_empty()
    {
        file.mime
    } else {
        "application/octet-stream"
    };
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{safe_name}\"\r\nContent-Type: {safe_mime}\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(file.bytes);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (boundary, out)
}

impl TelegramApi {
    pub fn new(base: Option<&str>) -> Self {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            // Disable pooling and transport retries so writes never silently retransmit.
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

    fn url(&self, token: &str, method: &str) -> String {
        format!("{}bot{token}/{method}", self.base)
    }

    pub async fn call_json(&self, method: &str, token: &str, body: &Value) -> Raw {
        let sent = self
            .http
            .post(self.url(token, method))
            .json(body)
            .send()
            .await;
        self.finish(sent, token).await
    }

    pub async fn get_updates(&self, token: &str, offset: Option<i64>, timeout_secs: u64) -> Raw {
        let mut body = serde_json::json!({
            "timeout": timeout_secs,
            "allowed_updates": ["message", "callback_query"],
        });
        if let Some(offset) = offset {
            body["offset"] = serde_json::json!(offset);
        }
        let sent = self
            .http
            .post(self.url(token, "getUpdates"))
            .timeout(Duration::from_secs(timeout_secs) + POLL_OVERHEAD)
            .json(&body)
            .send()
            .await;
        self.finish(sent, token).await
    }

    async fn send_media(
        &self,
        method: &str,
        field_name: &str,
        token: &str,
        chat_id: &str,
        file: MediaFile<'_>,
        options: MediaOptions<'_>,
    ) -> Raw {
        let (boundary, payload) = build_media_multipart(field_name, chat_id, file, options);
        let sent = self
            .http
            .post(self.url(token, method))
            .timeout(UPLOAD_TIMEOUT)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(payload)
            .send()
            .await;
        self.finish(sent, token).await
    }

    pub async fn send_document(
        &self,
        token: &str,
        chat_id: &str,
        file: MediaFile<'_>,
        options: MediaOptions<'_>,
    ) -> Raw {
        self.send_media("sendDocument", "document", token, chat_id, file, options)
            .await
    }

    pub async fn send_photo(
        &self,
        token: &str,
        chat_id: &str,
        file: MediaFile<'_>,
        options: MediaOptions<'_>,
    ) -> Raw {
        self.send_media("sendPhoto", "photo", token, chat_id, file, options)
            .await
    }

    /// Sets or clears (`emoji: None`) the bot's emoji reaction on a Telegram message.
    pub async fn set_message_reaction(
        &self,
        token: &str,
        chat_id: i64,
        message_id: u64,
        emoji: Option<&str>,
    ) -> Raw {
        let reaction = match emoji {
            Some(emoji) => serde_json::json!([{"type": "emoji", "emoji": emoji}]),
            None => serde_json::json!([]),
        };
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": reaction,
        });
        self.call_json("setMessageReaction", token, &body).await
    }

    pub async fn get_me(&self, token: &str) -> Option<BotInfo> {
        match self.call_json("getMe", token, &serde_json::json!({})).await {
            Raw::Ok(value) => {
                let id = value["id"].as_u64().filter(|id| *id > 0)?;
                let username = value["username"].as_str().map(str::to_owned);
                Some(BotInfo { id, username })
            }
            _ => None,
        }
    }

    async fn finish(&self, sent: Result<reqwest::Response, reqwest::Error>, token: &str) -> Raw {
        let response = match sent {
            Ok(response) => response,
            Err(error) if error.is_connect() => return Raw::BeforeWrite,
            Err(_) => return Raw::Unknown,
        };
        let status = response.status().as_u16();
        // Classify HTTP 409/401 immediately from status before waiting on or bounding the response body.
        if status == 409 {
            return Raw::Conflict;
        }
        if status == 401 {
            return Raw::TokenRejected;
        }
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        match response.bytes().await {
            Ok(body) => classify(status, retry_after.as_deref(), &body, token),
            Err(_) if status == 429 => classify(status, retry_after.as_deref(), b"", token),
            Err(_) if status == 409 => Raw::Conflict,
            Err(_) if status == 401 => Raw::TokenRejected,
            Err(_) => Raw::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ok_conflict_auth_rate_limit_and_scrubs_tokens() {
        let tok = "123456:SECRET_TOKEN_ABC";
        assert!(matches!(
            classify(200, None, br#"{"ok":true,"result":{"id":42}}"#, tok),
            Raw::Ok(v) if v["id"] == 42
        ));
        assert_eq!(
            classify(
                409,
                None,
                br#"{"ok":false,"error_code":409,"description":"Conflict: terminated by other getUpdates request"}"#,
                tok
            ),
            Raw::Conflict
        );
        assert_eq!(
            classify(401, None, br#"{"ok":false,"error_code":401}"#, tok),
            Raw::TokenRejected
        );
        let oversized = vec![b'x'; MAX_RESPONSE + 1];
        assert_eq!(classify(409, None, &oversized, tok), Raw::Conflict);
        assert_eq!(classify(401, None, &oversized, tok), Raw::TokenRejected);
        assert_eq!(
            classify(429, Some("9"), &oversized, tok),
            Raw::RateLimited(Duration::from_secs(9))
        );
        assert_eq!(
            classify(
                429,
                None,
                br#"{"ok":false,"error_code":429,"parameters":{"retry_after":7}}"#,
                tok
            ),
            Raw::RateLimited(Duration::from_secs(7))
        );
        assert_eq!(
            classify(502, None, br#"{"ok":true,"result":{}}"#, tok),
            Raw::Unknown
        );
        let err = classify(
            400,
            None,
            format!(r#"{{"ok":false,"error_code":400,"description":"bad {tok}"}}"#).as_bytes(),
            tok,
        );
        match err {
            Raw::Error(400, desc) => {
                assert!(!desc.contains(tok), "{desc}");
                assert!(desc.contains("[redacted]"), "{desc}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn multipart_builder_avoids_boundary_collisions_and_sanitizes_filename() {
        let (boundary, body) = build_media_multipart(
            "document",
            "-100777",
            MediaFile {
                name: "bad\"\r\nname.txt",
                mime: "text/plain",
                bytes: b"----botfw-tg-boundary-0000000000000000 payload",
            },
            MediaOptions {
                caption: Some("hello"),
                thread_id: Some(12),
                reply_to_message_id: Some(34),
            },
        );
        assert_ne!(boundary, "----botfw-tg-boundary-0000000000000000");
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("name=\"chat_id\"\r\n\r\n-100777\r\n"));
        assert!(text.contains("name=\"message_thread_id\"\r\n\r\n12\r\n"));
        assert!(text.contains("name=\"reply_parameters\"\r\n\r\n{\"allow_sending_without_reply\":true,\"message_id\":34}\r\n"));
        assert!(text.contains("name=\"caption\"\r\n\r\nhello\r\n"));
        assert!(text.contains("filename=\"badname.txt\""));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    }
}
