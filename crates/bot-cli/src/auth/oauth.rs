//! OAuth 2.0 PKCE S256, RFC 9728/8414/7591 discovery and registration, refresh, and callback capture.

use super::{
    http::{AuthHttp, form_encode, percent_encode},
    payload::{BodyFormat, token_value_ok},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bot_broker::{mcp::remote::Remote, resolve::HostPort};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const MIN_EXPIRES_SECS: i64 = 60;
const MAX_EXPIRES_SECS: i64 = 86_400 * 30;

#[derive(Clone, Debug)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

#[derive(Clone, Debug)]
pub struct DiscoveredOAuth {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub resource: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in_secs: Option<i64>,
    pub scope: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshError {
    InvalidGrant,
    Transient(&'static str),
}

pub fn pkce_pair() -> Result<PkcePair, String> {
    let rng = SystemRandom::new();
    let mut raw = [0u8; 32];
    rng.fill(&mut raw)
        .map_err(|_| "failed to generate PKCE verifier".to_owned())?;
    let verifier = URL_SAFE_NO_PAD.encode(raw);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok(PkcePair {
        verifier,
        challenge,
    })
}

pub fn random_state() -> Result<String, String> {
    let rng = SystemRandom::new();
    let mut raw = [0u8; 24];
    rng.fill(&mut raw)
        .map_err(|_| "failed to generate OAuth state".to_owned())?;
    Ok(URL_SAFE_NO_PAD.encode(raw))
}

/// Extracts `resource_metadata="..."` from an RFC 9728 `WWW-Authenticate` header.
pub fn parse_resource_metadata_header(header: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let idx = lower.find("resource_metadata=")?;
    let rest = &header[idx + "resource_metadata=".len()..];
    if let Some(quoted) = rest.strip_prefix('"') {
        let end = quoted.find('"')?;
        Some(quoted[..end].to_owned())
    } else {
        let end = rest.find([',', ' ', ';']).unwrap_or(rest.len());
        let val = rest[..end].trim();
        (!val.is_empty()).then(|| val.to_owned())
    }
}

/// Discovers authorization and token endpoints for a remote MCP server (RFC 9728 + RFC 8414).
pub async fn discover_mcp_oauth(
    http: &AuthHttp,
    remote: &Remote,
    profile_egress: Option<&[HostPort]>,
) -> Result<DiscoveredOAuth, String> {
    let resource_url = remote.url();
    let header = http
        .probe_www_authenticate(&resource_url, profile_egress)
        .await?;
    let scheme = if remote.tls { "https" } else { "http" };
    let default_port = if remote.tls { 443 } else { 80 };
    let origin = if remote.port == default_port {
        format!("{scheme}://{}", remote.host)
    } else {
        format!("{scheme}://{}:{}", remote.host, remote.port)
    };
    let clean_path = remote.path.split('?').next().unwrap_or("/");
    let fallback_pr = if clean_path == "/" {
        format!("{origin}/.well-known/oauth-protected-resource")
    } else {
        format!(
            "{origin}/.well-known/oauth-protected-resource{}",
            clean_path
        )
    };
    let pr_url = header
        .as_deref()
        .and_then(parse_resource_metadata_header)
        .unwrap_or(fallback_pr);
    let (status, pr_doc) = http.get_json(&pr_url, profile_egress).await?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "protected resource metadata request failed with HTTP {status}"
        ));
    }
    let resource = pr_doc
        .get("resource")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or(resource_url);
    let issuer = pr_doc
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(Value::as_str)
        .ok_or("protected resource metadata missing `authorization_servers`")?
        .trim_end_matches('/')
        .to_owned();
    let (as_remote, _) = http.vet_url(&issuer, profile_egress)?;
    let as_meta_url = auth_server_metadata_url(&as_remote);
    let (as_status, as_doc) = http.get_json(&as_meta_url, profile_egress).await?;
    if !(200..300).contains(&as_status) {
        return Err(format!(
            "authorization server metadata request failed with HTTP {as_status}"
        ));
    }
    if let Some(doc_issuer) = as_doc.get("issuer").and_then(Value::as_str)
        && doc_issuer.trim_end_matches('/') != issuer
    {
        return Err("authorization server metadata `issuer` mismatch".to_owned());
    }
    let authorization_endpoint = as_doc
        .get("authorization_endpoint")
        .and_then(Value::as_str)
        .ok_or("authorization server metadata missing `authorization_endpoint`")?
        .to_owned();
    let token_endpoint = as_doc
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or("authorization server metadata missing `token_endpoint`")?
        .to_owned();
    let registration_endpoint = as_doc
        .get("registration_endpoint")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // Validate every discovered endpoint against HTTPS + egress policy before returning.
    http.vet_url(&authorization_endpoint, profile_egress)?;
    http.vet_url(&token_endpoint, profile_egress)?;
    if let Some(reg) = &registration_endpoint {
        http.vet_url(reg, profile_egress)?;
    }
    Ok(DiscoveredOAuth {
        issuer,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        resource: Some(resource),
    })
}

/// Builds RFC 8414 §3 well-known metadata URL from a validated issuer endpoint.
fn auth_server_metadata_url(remote: &Remote) -> String {
    let scheme = if remote.tls { "https" } else { "http" };
    let default_port = if remote.tls { 443 } else { 80 };
    let origin = if remote.port == default_port {
        format!("{scheme}://{}", remote.host)
    } else {
        format!("{scheme}://{}:{}", remote.host, remote.port)
    };
    let clean_path = remote
        .path
        .split('?')
        .next()
        .unwrap_or("/")
        .trim_end_matches('/');
    if clean_path.is_empty() {
        format!("{origin}/.well-known/oauth-authorization-server")
    } else {
        format!("{origin}/.well-known/oauth-authorization-server{clean_path}")
    }
}

/// RFC 7591 dynamic client registration for public PKCE native clients.
pub async fn register_dynamic_client(
    http: &AuthHttp,
    registration_endpoint: &str,
    redirect_uri: &str,
    profile_egress: Option<&[HostPort]>,
) -> Result<(String, Option<String>), String> {
    let body = json!({
        "client_name": "bot-framework",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let (status, doc) = http
        .post_json(registration_endpoint, &body, profile_egress)
        .await?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "dynamic client registration failed with HTTP {status}"
        ));
    }
    let client_id = doc
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("dynamic client registration missing `client_id`")?
        .to_owned();
    let client_secret = doc
        .get("client_secret")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Ok((client_id, client_secret))
}

/// Builds the browser authorization URL with PKCE S256 and CSRF state.
pub fn build_authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: Option<&str>,
    resource: Option<&str>,
    state: &str,
    challenge: &str,
) -> String {
    let mut pairs = vec![
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    if let Some(s) = scope
        && !s.trim().is_empty()
    {
        pairs.push(("scope", s));
    }
    if let Some(r) = resource
        && !r.trim().is_empty()
    {
        pairs.push(("resource", r));
    }
    let sep = if authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{authorization_endpoint}{sep}{}", form_encode(&pairs))
}

/// Exchanges an authorization code + PKCE verifier at `token_endpoint`.
#[allow(clippy::too_many_arguments)]
pub async fn exchange_code(
    http: &AuthHttp,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    resource: Option<&str>,
    profile_egress: Option<&[HostPort]>,
) -> Result<TokenResponse, String> {
    let mut pairs = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];
    if let Some(cs) = client_secret {
        pairs.push(("client_secret", cs));
    }
    if let Some(res) = resource {
        pairs.push(("resource", res));
    }
    let (status, doc) = http
        .post_form(token_endpoint, &pairs, profile_egress)
        .await?;
    if !(200..300).contains(&status) {
        let code = extract_error_code(&doc).unwrap_or("token_exchange_failed");
        return Err(format!("token exchange failed (HTTP {status}, {code})"));
    }
    parse_token_response(&doc)
        .ok_or_else(|| "token response missing valid `access_token`".to_owned())
}

/// Refreshes an OAuth access token using RFC 6749 §6 (form or JSON body).
#[allow(clippy::too_many_arguments)]
pub async fn refresh_token(
    http: &AuthHttp,
    token_endpoint: &str,
    body_format: BodyFormat,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
    scope: Option<&str>,
    resource: Option<&str>,
    profile_egress: Option<&[HostPort]>,
    register_secret: &(dyn Fn(&str) + Send + Sync),
) -> Result<TokenResponse, RefreshError> {
    let (status, doc) = match body_format {
        BodyFormat::Form => {
            let mut pairs = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", client_id),
            ];
            if let Some(cs) = client_secret {
                pairs.push(("client_secret", cs));
            }
            if let Some(s) = scope {
                pairs.push(("scope", s));
            }
            if let Some(r) = resource {
                pairs.push(("resource", r));
            }
            http.post_form(token_endpoint, &pairs, profile_egress)
                .await
                .map_err(|e| classify_http_err(&e))?
        }
        BodyFormat::Json => {
            let mut obj = serde_json::Map::new();
            obj.insert("grant_type".into(), json!("refresh_token"));
            obj.insert("refresh_token".into(), json!(refresh_token));
            obj.insert("client_id".into(), json!(client_id));
            if let Some(cs) = client_secret {
                obj.insert("client_secret".into(), json!(cs));
            }
            if let Some(s) = scope {
                obj.insert("scope".into(), json!(s));
            }
            if let Some(r) = resource {
                obj.insert("resource".into(), json!(r));
            }
            http.post_json(token_endpoint, &Value::Object(obj), profile_egress)
                .await
                .map_err(|e| classify_http_err(&e))?
        }
    };
    if (200..300).contains(&status) {
        let parsed = parse_token_response(&doc).ok_or(RefreshError::Transient("bad_response"))?;
        register_secret(&parsed.access_token);
        if let Some(rt) = &parsed.refresh_token {
            register_secret(rt);
        }
        return Ok(parsed);
    }
    if is_permanent_grant_error(&doc) {
        Err(RefreshError::InvalidGrant)
    } else {
        Err(RefreshError::Transient("http_error"))
    }
}

fn classify_http_err(msg: &str) -> RefreshError {
    if msg.contains("egress") || msg.contains("https") || msg.contains("disallowed") {
        RefreshError::Transient("egress_denied")
    } else {
        RefreshError::Transient("network")
    }
}

fn parse_token_response(doc: &Value) -> Option<TokenResponse> {
    let access_token = doc
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| token_value_ok(t))?
        .to_owned();
    let refresh_token = doc
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|t| token_value_ok(t))
        .map(str::to_owned);
    let expires_in_secs = doc
        .get("expires_in")
        .and_then(Value::as_i64)
        .map(|s| s.clamp(MIN_EXPIRES_SECS, MAX_EXPIRES_SECS));
    let scope = doc.get("scope").and_then(Value::as_str).map(str::to_owned);
    Some(TokenResponse {
        access_token,
        refresh_token,
        expires_in_secs,
        scope,
    })
}

fn extract_error_code(doc: &Value) -> Option<&str> {
    if let Some(s) = doc.get("error").and_then(Value::as_str) {
        return Some(s);
    }
    doc.get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
}

fn is_permanent_grant_error(doc: &Value) -> bool {
    matches!(
        extract_error_code(doc),
        Some(
            "invalid_grant"
                | "refresh_token_reused"
                | "refresh_token_expired"
                | "refresh_token_invalidated"
        )
    )
}

/// Verifies callback query `state` in constant time and extracts `code`.
pub fn parse_callback_query(query: &str, expected_state: &str) -> Result<String, String> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let dec = percent_decode(v);
        match k {
            "code" => code = Some(dec),
            "state" => state = Some(dec),
            "error" => error = Some(dec),
            _ => {}
        }
    }
    let Some(got_state) = state else {
        return Err("OAuth callback missing `state` parameter".to_owned());
    };
    if !constant_time_eq(got_state.as_bytes(), expected_state.as_bytes()) {
        return Err("OAuth callback `state` mismatch (possible CSRF)".to_owned());
    }
    if let Some(err) = error {
        return Err(format!("OAuth provider returned error `{err}`"));
    }
    code.filter(|c| !c.is_empty())
        .ok_or_else(|| "OAuth callback missing `code` parameter".to_owned())
}

/// Parses a full pasted redirect URL (`http://127.0.0.1:.../callback?code=...&state=...`).
pub fn parse_pasted_callback(input: &str, expected_state: &str) -> Result<String, String> {
    let trimmed = input.trim();
    let (_, query) = trimmed
        .split_once('?')
        .ok_or("pasted redirect URL must include `?code=...&state=...`")?;
    let query = query.split('#').next().unwrap_or(query);
    parse_callback_query(query, expected_state)
}

/// Waits for a loopback HTTP GET `/callback?...` request (ignoring preconnects/non-callback paths) and validates `state`.
pub async fn await_loopback_callback(
    listener: TcpListener,
    expected_state: &str,
    wait: Duration,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let (mut stream, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| "timed out waiting for OAuth browser callback".to_owned())?
            .map_err(|_| "failed to accept loopback callback".to_owned())?;
        let mut buf = vec![0u8; 8192];
        let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await
        else {
            continue;
        };
        if n == 0 {
            continue;
        }
        let text = std::str::from_utf8(&buf[..n]).unwrap_or("");
        let first_line = text.lines().next().unwrap_or("");
        let target = first_line.split_whitespace().nth(1).unwrap_or("");
        let path = target.split('?').next().unwrap_or("");
        if path != "/callback" {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let result = target
            .split_once('?')
            .ok_or_else(|| "callback missing query string".to_owned())
            .and_then(|(_, q)| parse_callback_query(q, expected_state));
        let (status, body) = match &result {
            Ok(_) => ("200 OK", "Authorization received. You may close this tab."),
            Err(_) => ("400 Bad Request", "Authorization failed or state mismatch."),
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(reply.as_bytes()).await;
        return result;
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
                && let Ok(val) = u8::from_str_radix(hex, 16)
            {
                out.push(val);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn encoded_state_hint(state: &str) -> String {
    percent_encode(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_state_and_www_authenticate_parsing_work() {
        let pair = pkce_pair().unwrap();
        assert_eq!(pair.verifier.len(), 43);
        assert_eq!(pair.challenge.len(), 43);
        assert_eq!(
            parse_resource_metadata_header(
                r#"Bearer realm="mcp", resource_metadata="https://mcp.example/.well-known/oauth-protected-resource""#
            ).as_deref(),
            Some("https://mcp.example/.well-known/oauth-protected-resource")
        );
        let st = random_state().unwrap();
        assert_eq!(
            parse_pasted_callback(
                &format!("http://127.0.0.1:9999/callback?code=abc%201&state={st}"),
                &st
            )
            .unwrap(),
            "abc 1"
        );
        assert!(
            parse_pasted_callback("http://127.0.0.1:9999/callback?code=abc&state=wrong", &st)
                .is_err()
        );
    }

    #[test]
    fn classifies_permanent_vs_transient_oauth_errors() {
        assert!(is_permanent_grant_error(&json!({"error": "invalid_grant"})));
        assert!(is_permanent_grant_error(
            &json!({"error": {"code": "refresh_token_reused"}})
        ));
        assert!(!is_permanent_grant_error(
            &json!({"error": "temporarily_unavailable"})
        ));
        assert!(!is_permanent_grant_error(
            &json!({"error": "rate_limit_exceeded"})
        ));
    }
}
